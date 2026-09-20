//! Finding validation, fingerprinting, and duplicate grouping (plan
//! sections 14, 15, 37, and 38).
//!
//! Reviewer reports are untrusted structured output. Before any finding
//! may influence a review result, Rust validates it deterministically:
//!
//! - **schema shape and text bounds** — identifiers, categories, titles,
//!   claims, explanations, recommendations, and excerpts are non-empty
//!   where required and bounded;
//! - **reviewer identity** — the report must come from the juror Rust
//!   expected for it, and finding IDs must be unique within the report;
//! - **severity bounds** — the proposed severity is clamped to the frozen
//!   maximum-severity policy of its impact class (the clamped value is
//!   what survives; reviewers never set final severity);
//! - **evidence existence and snapshot membership** — source evidence
//!   must read back from the exact target snapshot (with content keys
//!   proven against the snapshot's content), diff evidence must name a
//!   changed path of the review scope, and project-fact evidence must
//!   reference an existing durable record;
//! - **evidence requirement** — high/blocker findings require at least
//!   one concrete valid evidence entry.
//!
//! Invalid findings are excluded from consolidation and their diagnostic
//! information is retained ([`RejectedFinding`]); validation never
//! rewrites an invalid finding into a guessed one.
//!
//! Valid findings receive a deterministic fingerprint. Identical
//! fingerprints group "obvious duplicates" (plan section 38 step 3);
//! cross-reviewer pairs that share an evidence path but differ otherwise
//! become equivalence-check candidates (step 5), decided separately by
//! the narrow Pi equivalence check.

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest as _, Sha256};

use crate::domain::evidence::{EvidenceKind, EvidenceRef};
use crate::domain::finding::{ReviewerFinding, SynthesisDiagnostic, SynthesisStage};
use crate::domain::report::ReviewerReport;
use crate::domain::review::{ImpactClass, Severity};
use crate::git::{ObjectFormat, SnapshotFile, SnapshotReader};
use crate::index::content::{git_blob_oid, working_tree_key};
use crate::jury::protocol::{EquivalenceVerdict, FindingEquivalenceOutcome};

/// Maximum byte length of a reviewer-local finding identifier.
pub const MAX_FINDING_ID_BYTES: usize = 128;
/// Maximum byte length of a finding category.
pub const MAX_CATEGORY_BYTES: usize = 64;
/// Maximum byte length of a finding title.
pub const MAX_TITLE_BYTES: usize = 200;
/// Maximum byte length of a finding claim.
pub const MAX_CLAIM_BYTES: usize = 2_000;
/// Maximum byte length of a finding explanation.
pub const MAX_EXPLANATION_BYTES: usize = 4_000;
/// Maximum byte length of a finding recommendation.
pub const MAX_RECOMMENDATION_BYTES: usize = 2_000;
/// Maximum byte length of an evidence excerpt.
pub const MAX_EXCERPT_BYTES: usize = 1_000;
/// Maximum byte length of an evidence symbol.
pub const MAX_SYMBOL_BYTES: usize = 256;
/// Maximum byte length of an evidence path.
pub const MAX_PATH_BYTES: usize = 4_096;
/// Maximum number of evidence entries one finding may carry; entries
/// beyond this bound are dropped with a retained diagnostic.
pub const MAX_EVIDENCE_PER_FINDING: usize = 16;
/// Maximum number of findings accepted from one report; findings beyond
/// this bound are rejected so a runaway reviewer cannot flood a review.
pub const MAX_FINDINGS_PER_REPORT: usize = 200;
/// Number of claim bytes that participate in the fingerprint (a bound on
/// hash input, not on stored text).
const MAX_CLAIM_FINGERPRINT_BYTES: usize = 512;

/// Version prefix of the fingerprint input; fingerprints are only stable
/// within one fingerprint version.
const FINGERPRINT_VERSION: &str = "nisse-finding-fingerprint-v1";

/// Everything evidence validation needs to prove snapshot membership.
#[derive(Debug, Clone, Copy)]
pub struct ValidationContext<'a> {
    /// Reader of the review's target snapshot.
    pub reader: &'a SnapshotReader<'a>,
    /// Object format of the repository (for working-side `git:` keys).
    pub object_format: ObjectFormat,
    /// Paths changed by the review scope, when known. `None` means the
    /// scope is unknown and diff evidence is only checked structurally.
    pub changed_paths: Option<&'a BTreeSet<String>>,
    /// IDs of durable records that currently exist.
    pub known_record_ids: &'a BTreeSet<String>,
}

/// One validated reviewer finding, ready for consolidation.
///
/// The stored finding is the cleaned result of validation: only valid
/// evidence entries survive and `proposed_severity` has been replaced by
/// the policy-clamped severity (Rust applies severity policy; plan
/// section 14).
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedFinding {
    /// Reviewer that produced the finding.
    pub reviewer_id: String,
    /// Cleaned finding with policy-clamped severity and valid evidence.
    pub finding: ReviewerFinding,
    /// Deterministic fingerprint of the cleaned finding.
    pub fingerprint: String,
}

impl ValidatedFinding {
    /// The validated (policy-clamped) severity of this finding.
    pub fn severity(&self) -> Severity {
        self.finding.proposed_severity
    }

    /// The impact class of this finding.
    pub fn impact_class(&self) -> ImpactClass {
        self.finding.impact_class
    }

    /// Repository-relative evidence paths (source and diff kinds) that
    /// back this finding.
    pub fn evidence_paths(&self) -> BTreeSet<String> {
        self.finding
            .evidence
            .iter()
            .filter(|e| matches!(e.kind, EvidenceKind::Source | EvidenceKind::Diff))
            .filter_map(|e| e.path.clone())
            .collect()
    }

    /// Reviewer-qualified finding identifier (`<reviewer>/<id>`), unique
    /// within a review because jurors are unique and finding IDs are
    /// unique within one report.
    pub fn qualified_id(&self) -> String {
        format!("{}/{}", self.reviewer_id, self.finding.id)
    }
}

/// Diagnostic information for one excluded finding (plan section 37:
/// invalid findings are excluded and diagnostic information retained).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedFinding {
    /// Reviewer whose report contained the finding.
    pub reviewer_id: String,
    /// Reviewer-local finding identifier, when parseable from the input.
    pub finding_id: Option<String>,
    /// Why the finding was excluded.
    pub reason: String,
}

/// The outcome of validating one reviewer report.
#[derive(Debug, Clone, PartialEq)]
pub struct ReportValidation {
    /// Reviewer identity the report was validated against.
    pub reviewer_id: String,
    /// Findings that passed every validation.
    pub accepted: Vec<ValidatedFinding>,
    /// Excluded findings with retained diagnostics.
    pub rejected: Vec<RejectedFinding>,
}

/// Validate one reviewer report against the expected juror identity.
///
/// The report's `reviewer_id` must equal `expected_reviewer_id` (reviewer
/// identity is assigned by Rust through the execution spec; a mismatch
/// rejects every finding of the report). Findings are validated and
/// clamped as described in the module documentation; their relative order
/// is preserved.
pub fn validate_report(
    context: &ValidationContext<'_>,
    expected_reviewer_id: &str,
    report: &ReviewerReport,
) -> ReportValidation {
    let mut validation = ReportValidation {
        reviewer_id: expected_reviewer_id.to_string(),
        accepted: Vec::new(),
        rejected: Vec::new(),
    };

    if expected_reviewer_id.is_empty() {
        validation.rejected.push(RejectedFinding {
            reviewer_id: expected_reviewer_id.to_string(),
            finding_id: None,
            reason: format!(
                "expected reviewer identity is empty; all {} finding(s) of the report excluded",
                report.findings.len()
            ),
        });
        return validation;
    }
    if report.reviewer_id != expected_reviewer_id {
        validation.rejected.push(RejectedFinding {
            reviewer_id: expected_reviewer_id.to_string(),
            finding_id: None,
            reason: format!(
                "report reviewer identity `{}` does not match the expected juror `{expected_reviewer_id}`; all {} finding(s) of the report excluded",
                report.reviewer_id,
                report.findings.len()
            ),
        });
        return validation;
    }

    let mut seen_ids: BTreeSet<String> = BTreeSet::new();
    for (index, finding) in report.findings.iter().enumerate() {
        if index >= MAX_FINDINGS_PER_REPORT {
            validation.rejected.push(RejectedFinding {
                reviewer_id: expected_reviewer_id.to_string(),
                finding_id: non_empty_id(finding),
                reason: format!("report exceeds the maximum of {MAX_FINDINGS_PER_REPORT} findings"),
            });
            continue;
        }
        if let Some(reason) = shape_error(finding) {
            validation.rejected.push(RejectedFinding {
                reviewer_id: expected_reviewer_id.to_string(),
                finding_id: non_empty_id(finding),
                reason,
            });
            continue;
        }
        if !seen_ids.insert(finding.id.clone()) {
            validation.rejected.push(RejectedFinding {
                reviewer_id: expected_reviewer_id.to_string(),
                finding_id: Some(finding.id.clone()),
                reason: format!("duplicate finding id `{}` within the report", finding.id),
            });
            continue;
        }

        // Severity policy: clamp before the evidence requirement so the
        // requirement applies to the severity the finding may actually
        // carry (a hallucinated blocker about style becomes a `low`
        // finding that needs no evidence).
        let clamped_severity = clamp_severity(finding.impact_class, finding.proposed_severity);

        // Evidence: keep only valid entries, retaining why invalid ones
        // were dropped.
        let mut valid_evidence = Vec::new();
        for evidence in &finding.evidence {
            if valid_evidence.len() >= MAX_EVIDENCE_PER_FINDING {
                validation.rejected.push(RejectedFinding {
                    reviewer_id: expected_reviewer_id.to_string(),
                    finding_id: Some(finding.id.clone()),
                    reason: format!(
                        "finding `{}` exceeds {MAX_EVIDENCE_PER_FINDING} evidence entries; excess entries dropped",
                        finding.id
                    ),
                });
                break;
            }
            match validate_evidence(context, evidence) {
                Ok(()) => valid_evidence.push(evidence.clone()),
                Err(reason) => validation.rejected.push(RejectedFinding {
                    reviewer_id: expected_reviewer_id.to_string(),
                    finding_id: Some(finding.id.clone()),
                    reason: format!(
                        "evidence entry rejected for finding `{}`: {reason}",
                        finding.id
                    ),
                }),
            }
        }

        if clamped_severity >= Severity::High && valid_evidence.is_empty() {
            validation.rejected.push(RejectedFinding {
                reviewer_id: expected_reviewer_id.to_string(),
                finding_id: Some(finding.id.clone()),
                reason: format!(
                    "finding `{}` carries {clamped_severity:?} severity but has no concrete valid evidence",
                    finding.id
                ),
            });
            continue;
        }

        let cleaned = ReviewerFinding {
            proposed_severity: clamped_severity,
            evidence: valid_evidence,
            ..finding.clone()
        };
        let fingerprint = finding_fingerprint(&cleaned);
        validation.accepted.push(ValidatedFinding {
            reviewer_id: expected_reviewer_id.to_string(),
            fingerprint,
            finding: cleaned,
        });
    }
    validation
}

/// Reviewer-local id when non-empty, else `None`.
fn non_empty_id(finding: &ReviewerFinding) -> Option<String> {
    (!finding.id.is_empty()).then(|| finding.id.clone())
}

/// Clamp a proposed severity to the policy maximum of an impact class.
pub fn clamp_severity(impact_class: ImpactClass, proposed: Severity) -> Severity {
    proposed.min(impact_class.max_severity())
}

/// Structural and text-bound checks that do not need the snapshot.
fn shape_error(finding: &ReviewerFinding) -> Option<String> {
    let bounded = |value: &str, bound: usize, field: &str| {
        (value.len() > bound)
            .then(|| format!("`{field}` exceeds {bound} bytes (got {})", value.len()))
    };
    if finding.id.is_empty() {
        return Some("`id` must be non-empty".to_string());
    }
    if finding.category.is_empty() {
        return Some("`category` must be non-empty".to_string());
    }
    if finding.title.is_empty() {
        return Some("`title` must be non-empty".to_string());
    }
    if finding.claim.is_empty() {
        return Some("`claim` must be non-empty".to_string());
    }
    bounded(&finding.id, MAX_FINDING_ID_BYTES, "id")
        .or_else(|| bounded(&finding.category, MAX_CATEGORY_BYTES, "category"))
        .or_else(|| bounded(&finding.title, MAX_TITLE_BYTES, "title"))
        .or_else(|| bounded(&finding.claim, MAX_CLAIM_BYTES, "claim"))
        .or_else(|| bounded(&finding.explanation, MAX_EXPLANATION_BYTES, "explanation"))
        .or_else(|| {
            bounded(
                &finding.recommendation,
                MAX_RECOMMENDATION_BYTES,
                "recommendation",
            )
        })
}

/// Validate one evidence entry against the target snapshot.
///
/// Returns `Err(reason)` when the entry must be excluded. Shared by
/// finding validation and deterministic fact-candidate validation
/// (plan sections 37 and 47).
pub(crate) fn validate_evidence(
    context: &ValidationContext<'_>,
    evidence: &EvidenceRef,
) -> Result<(), String> {
    if evidence.excerpt.is_empty() {
        return Err("`excerpt` must be non-empty".to_string());
    }
    if evidence.excerpt.len() > MAX_EXCERPT_BYTES {
        return Err(format!(
            "`excerpt` exceeds {MAX_EXCERPT_BYTES} bytes (got {})",
            evidence.excerpt.len()
        ));
    }
    if let Some(symbol) = &evidence.symbol
        && symbol.len() > MAX_SYMBOL_BYTES
    {
        return Err(format!("`symbol` exceeds {MAX_SYMBOL_BYTES} bytes"));
    }

    match evidence.kind {
        EvidenceKind::Source => validate_source_evidence(context, evidence),
        EvidenceKind::Diff => {
            let Some(path) = &evidence.path else {
                return Err("diff evidence requires `path`".to_string());
            };
            if path.is_empty() || path.len() > MAX_PATH_BYTES {
                return Err("`path` must be non-empty and bounded".to_string());
            }
            if let Some(changed) = context.changed_paths
                && !changed.contains(path)
            {
                return Err(format!(
                    "diff evidence path `{path}` is not part of the review scope"
                ));
            }
            validate_range_shape(evidence)
        }
        EvidenceKind::Plan => {
            if let Some(section) = &evidence.plan_section
                && (section.is_empty() || section.len() > MAX_CATEGORY_BYTES)
            {
                return Err("`plan_section` must be non-empty and bounded".to_string());
            }
            validate_range_shape(evidence)
        }
        EvidenceKind::ProjectFact => {
            let Some(record_id) = &evidence.record_id else {
                return Err("project-fact evidence requires `record_id`".to_string());
            };
            if record_id.is_empty() || record_id.len() > MAX_FINDING_ID_BYTES {
                return Err("`record_id` must be non-empty and bounded".to_string());
            }
            if !context.known_record_ids.contains(record_id) {
                return Err(format!("referenced record `{record_id}` does not exist"));
            }
            validate_range_shape(evidence)
        }
    }
}

/// Source evidence must read back from the exact target snapshot, with a
/// content key proven against the snapshot's content and ranges valid
/// against the real line count (plan sections 15 and 26).
fn validate_source_evidence(
    context: &ValidationContext<'_>,
    evidence: &EvidenceRef,
) -> Result<(), String> {
    let Some(path) = &evidence.path else {
        return Err("source evidence requires `path`".to_string());
    };
    if path.is_empty() || path.len() > MAX_PATH_BYTES {
        return Err("`path` must be non-empty and bounded".to_string());
    }
    let file = context
        .reader
        .read(path)
        .map_err(|err| format!("source read failed for `{path}`: {err}"))?
        .ok_or_else(|| format!("source path `{path}` is not part of the target snapshot"))?;

    if let Some(content_key) = &evidence.content_key
        && !snapshot_content_keys(&file, context.object_format).contains(content_key)
    {
        return Err(format!(
            "content key `{content_key}` does not match the target snapshot content of `{path}`"
        ));
    }

    validate_range_shape(evidence)?;
    if evidence.line_start.is_some() || evidence.line_end.is_some() {
        let lines = line_count(&file.bytes);
        let start = evidence.line_start.unwrap_or(1);
        let end = evidence.line_end.unwrap_or(start);
        if end > lines || start > lines {
            return Err(format!(
                "line range {start}..{end} exceeds the {lines} line(s) of `{path}` in the target snapshot"
            ));
        }
    }
    Ok(())
}

/// Every content key the snapshot content of a file legitimately answers
/// to: the reader-side blob OID when the side defines one, the blob OID
/// the exact bytes would hash to, and the working-tree key of the bytes.
fn snapshot_content_keys(file: &SnapshotFile, format: ObjectFormat) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    keys.insert(working_tree_key(&file.bytes).as_str());
    keys.insert(format!("git:{}", git_blob_oid(&file.bytes, format)));
    if let Some(oid) = &file.blob_oid {
        keys.insert(format!("git:{oid}"));
    }
    keys
}

/// Range shape check shared by every evidence kind: 1-based, ordered.
fn validate_range_shape(evidence: &EvidenceRef) -> Result<(), String> {
    if let (Some(start), Some(end)) = (evidence.line_start, evidence.line_end)
        && (start < 1 || end < start)
    {
        return Err(format!(
            "line range must be 1-based with line_start <= line_end, got {start}..{end}"
        ));
    }
    if evidence.line_start.is_none() && evidence.line_end.is_some() {
        return Err("`line_end` requires `line_start`".to_string());
    }
    Ok(())
}

/// Number of lines of byte content (a trailing incomplete line counts).
fn line_count(bytes: &[u8]) -> u32 {
    if bytes.is_empty() {
        return 0;
    }
    let mut lines = bytes.iter().filter(|b| **b == b'\n').count() as u32;
    if bytes.last() != Some(&b'\n') {
        lines += 1;
    }
    lines
}

/// Compute the deterministic fingerprint of a cleaned finding.
///
/// The fingerprint hashes the impact class, normalized category and
/// claim, and the primary (first) evidence location. Identical
/// fingerprints are "obvious duplicates" (plan section 38 step 3);
/// findings that merely share evidence locations become equivalence
/// candidates instead.
pub fn finding_fingerprint(finding: &ReviewerFinding) -> String {
    let primary = finding
        .evidence
        .first()
        .map(primary_evidence_descriptor)
        .unwrap_or_else(|| "none".to_string());
    let input = format!(
        "{FINGERPRINT_VERSION}\nimpact={}\ncategory={}\nclaim={}\nevidence={primary}",
        serde_json::to_string(&finding.impact_class).expect("impact class serializes"),
        normalize_text(&finding.category),
        normalize_text(&truncate_char_safe(
            &finding.claim,
            MAX_CLAIM_FINGERPRINT_BYTES
        )),
    );
    to_hex(&Sha256::digest(input.as_bytes()))
}

/// Stable descriptor of the primary evidence location.
fn primary_evidence_descriptor(evidence: &EvidenceRef) -> String {
    let field =
        |option: &Option<String>| option.as_deref().unwrap_or_default().trim().to_lowercase();
    format!(
        "{}|{}|{}|{:?}",
        serde_json::to_string(&evidence.kind).expect("evidence kind serializes"),
        field(&evidence.path),
        field(&evidence.symbol),
        evidence.line_start
    )
}

/// Lowercase, trim, and collapse internal whitespace.
fn normalize_text(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Truncate to at most `limit` bytes without splitting a character.
fn truncate_char_safe(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut boundary = limit;
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text[..boundary].to_string()
}

/// Lowercase hex encoding.
fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// A group of validated findings consolidated into one final finding.
///
/// Members share one impact class (fingerprints include it) and their
/// order is deterministic: the order findings were validated in.
#[derive(Debug, Clone, PartialEq)]
pub struct FindingGroup {
    /// Group key: the fingerprint of the group's first member. Used as
    /// the stable identifier the synthesis session must reference.
    pub key: String,
    /// Member findings in deterministic order.
    pub members: Vec<ValidatedFinding>,
}

impl FindingGroup {
    /// The impact class shared by every member.
    pub fn impact_class(&self) -> ImpactClass {
        self.members[0].impact_class()
    }

    /// The highest member severity (the group's policy ceiling).
    pub fn max_severity(&self) -> Severity {
        self.members
            .iter()
            .map(|member| member.severity())
            .max()
            .unwrap_or(Severity::Info)
    }

    /// The member used for deterministic fallback wording: highest
    /// severity, ties broken by member order.
    pub fn top_member(&self) -> &ValidatedFinding {
        let mut top = &self.members[0];
        for member in &self.members[1..] {
            if member.severity() > top.severity() {
                top = member;
            }
        }
        top
    }

    /// Distinct reviewer IDs of the members, sorted.
    pub fn reviewer_ids(&self) -> Vec<String> {
        let set: BTreeSet<&str> = self
            .members
            .iter()
            .map(|member| member.reviewer_id.as_str())
            .collect();
        set.into_iter().map(str::to_string).collect()
    }

    /// Whether equivalent findings came from at least two independent
    /// reviewers (plan section 38).
    pub fn cross_validated(&self) -> bool {
        self.reviewer_ids().len() >= 2
    }

    /// Union of member evidence paths (source and diff kinds).
    fn evidence_paths(&self) -> BTreeSet<String> {
        self.members
            .iter()
            .flat_map(|member| member.evidence_paths())
            .collect()
    }
}

/// Group validated findings by identical fingerprints ("obvious
/// duplicates", plan section 38 step 3).
///
/// Groups appear in the order their first member was validated in;
/// members keep validation order. Grouping is pure and deterministic.
pub fn group_findings(findings: &[ValidatedFinding]) -> Vec<FindingGroup> {
    let mut order: Vec<String> = Vec::new();
    let mut by_fingerprint: BTreeMap<String, Vec<ValidatedFinding>> = BTreeMap::new();
    for finding in findings {
        if !by_fingerprint.contains_key(&finding.fingerprint) {
            order.push(finding.fingerprint.clone());
        }
        by_fingerprint
            .entry(finding.fingerprint.clone())
            .or_default()
            .push(finding.clone());
    }
    order
        .into_iter()
        .map(|key| FindingGroup {
            members: by_fingerprint.remove(&key).unwrap_or_default(),
            key,
        })
        .collect()
}

/// Deterministic candidate group pairs for the narrow equivalence check
/// (plan section 38 step 5).
///
/// A pair of groups is a candidate when:
///
/// - at least one cross-reviewer member pair exists (same-reviewer
///   near-duplicates stay separate findings);
/// - the groups share an impact class and at least one evidence path
///   (source or diff), so both point at the same location;
/// - their keys differ (identical fingerprints were already grouped).
///
/// Pairs are returned in ascending `(index, index)` order.
pub fn equivalence_candidates(groups: &[FindingGroup]) -> Vec<(usize, usize)> {
    let mut pairs = Vec::new();
    for (i, a) in groups.iter().enumerate() {
        for (j, b) in groups.iter().enumerate().skip(i + 1) {
            if a.impact_class() != b.impact_class() {
                continue;
            }
            let cross_reviewer = a
                .members
                .iter()
                .any(|ma| b.members.iter().any(|mb| mb.reviewer_id != ma.reviewer_id));
            if !cross_reviewer {
                continue;
            }
            let paths_a = a.evidence_paths();
            if b.evidence_paths().iter().any(|path| paths_a.contains(path)) {
                pairs.push((i, j));
            }
        }
    }
    pairs
}

/// Union-find root with path halving.
fn find_root(parent: &mut [usize], mut node: usize) -> usize {
    while parent[node] != node {
        let grandparent = parent[parent[node]];
        parent[node] = grandparent;
        node = grandparent;
    }
    node
}

/// Apply equivalence outcomes to grouped findings (plan section 38
/// step 6: group only `same` findings).
///
/// `pairs` and `outcomes` must be parallel (one outcome per pair, in the
/// order returned by [`equivalence_candidates`]). `same` merges the two
/// groups (union-find; the merged group keeps the earlier group's key
/// and member order); `different`, `uncertain`, and errored checks never
/// merge. Errored checks additionally record a diagnostic.
pub fn apply_equivalence(
    groups: Vec<FindingGroup>,
    pairs: &[(usize, usize)],
    outcomes: &[FindingEquivalenceOutcome],
    diagnostics: &mut Vec<SynthesisDiagnostic>,
) -> Vec<FindingGroup> {
    debug_assert_eq!(
        pairs.len(),
        outcomes.len(),
        "equivalence pairs and outcomes must be parallel"
    );
    let mut parent: Vec<usize> = (0..groups.len()).collect();

    for ((a, b), outcome) in pairs.iter().zip(outcomes) {
        if outcome.verdict == EquivalenceVerdict::Same {
            let (root_a, root_b) = (find_root(&mut parent, *a), find_root(&mut parent, *b));
            // The smaller root wins so merged groups keep the earlier key.
            if root_a != root_b {
                if root_b < root_a {
                    parent[root_a] = root_b;
                } else {
                    parent[root_b] = root_a;
                }
            }
        } else if outcome.verdict == EquivalenceVerdict::Uncertain
            && let Some(error) = &outcome.error
        {
            diagnostics.push(SynthesisDiagnostic::new(
                SynthesisStage::Equivalence,
                format!(
                    "equivalence check `{}` failed ({}: {}); findings left ungrouped",
                    outcome.check_id, error.code, error.message
                ),
            ));
        }
    }

    let mut merged: BTreeMap<usize, Vec<ValidatedFinding>> = BTreeMap::new();
    for (index, group) in groups.into_iter().enumerate() {
        let root = find_root(&mut parent, index);
        merged.entry(root).or_default().extend(group.members);
    }
    merged
        .into_values()
        .map(|members| FindingGroup {
            // Members begin with the root group's first member because
            // groups are folded in ascending index order.
            key: members[0].fingerprint.clone(),
            members,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::evidence::{EvidenceKind, EvidenceRef};
    use crate::domain::review::{ImpactClass, Severity};
    use crate::git::resolve_snapshot;
    use crate::git::testutil::{Fixture, commit_all, write_file};
    use crate::git::{GitRunner, RepoContext, ResolvedSnapshot, SnapshotReader, SnapshotSpec};
    use crate::index::content::git_blob_oid;
    use crate::jury::protocol::{EquivalenceVerdict, HostErrorInfo};

    fn evidence(path: &str, excerpt: &str) -> EvidenceRef {
        EvidenceRef {
            kind: EvidenceKind::Source,
            path: Some(path.to_string()),
            symbol: None,
            line_start: None,
            line_end: None,
            content_key: None,
            plan_section: None,
            record_id: None,
            excerpt: excerpt.to_string(),
        }
    }

    fn finding(id: &str, impact: ImpactClass, severity: Severity, claim: &str) -> ReviewerFinding {
        ReviewerFinding {
            id: id.to_string(),
            impact_class: impact,
            proposed_severity: severity,
            category: "logic".to_string(),
            title: format!("Finding {id}"),
            claim: claim.to_string(),
            explanation: "Because.".to_string(),
            recommendation: "Fix it.".to_string(),
            evidence: vec![evidence("src/a.rs", "fn a() {}")],
        }
    }

    fn report(reviewer: &str, findings: Vec<ReviewerFinding>) -> ReviewerReport {
        ReviewerReport {
            reviewer_id: reviewer.to_string(),
            summary: "s".to_string(),
            findings,
            candidate_project_facts: Vec::new(),
            model_metadata: serde_json::Value::Null,
            duration_ms: 0,
        }
    }

    /// A fixture repository whose working snapshot contains `src/a.rs`
    /// (3 lines) and `src/b.rs`. The returned values are leaked so the
    /// borrowed reader can outlive the setup call.
    struct Repo {
        _fixture: Fixture,
        ctx: &'static RepoContext,
        resolved: &'static ResolvedSnapshot,
    }

    fn setup(name: &str) -> Repo {
        let fixture = Fixture::repo(name);
        write_file(&fixture.root, "src/a.rs", "fn a() {\n    marker\n}\n");
        write_file(&fixture.root, "src/b.rs", "fn b() {}\n");
        commit_all(&fixture.root, "one");
        let ctx: &'static RepoContext = Box::leak(Box::new(
            RepoContext::discover(&GitRunner::new(), &fixture.root).unwrap(),
        ));
        let resolved: &'static ResolvedSnapshot = Box::leak(Box::new(
            resolve_snapshot(ctx, &SnapshotSpec::Working).unwrap(),
        ));
        Repo {
            _fixture: fixture,
            ctx,
            resolved,
        }
    }

    impl Repo {
        fn context<'r>(
            &'r self,
            changed_paths: Option<&'r BTreeSet<String>>,
            records: &'r BTreeSet<String>,
        ) -> ValidationContext<'r> {
            let reader = SnapshotReader::new(self.ctx, self.resolved).unwrap();
            ValidationContext {
                reader: Box::leak(Box::new(reader)),
                object_format: ObjectFormat::Sha1,
                changed_paths,
                known_record_ids: records,
            }
        }
    }

    #[test]
    fn shape_and_identity_validation() {
        let repo = setup("val-shape");
        let records = BTreeSet::new();
        let context = repo.context(None, &records);

        // Identity mismatch rejects every finding of the report.
        let validation = validate_report(
            &context,
            "quality",
            &report(
                "impostor",
                vec![finding(
                    "f-1",
                    ImpactClass::Correctness,
                    Severity::Info,
                    "claim",
                )],
            ),
        );
        assert!(validation.accepted.is_empty());
        assert_eq!(validation.rejected.len(), 1);
        assert_eq!(validation.rejected[0].reviewer_id, "quality");
        assert!(validation.rejected[0].reason.contains("does not match"));

        // Duplicate finding ids: first wins, second rejected.
        let validation = validate_report(
            &context,
            "quality",
            &report(
                "quality",
                vec![
                    finding("f-1", ImpactClass::Correctness, Severity::Info, "one"),
                    finding("f-1", ImpactClass::Correctness, Severity::Info, "two"),
                ],
            ),
        );
        assert_eq!(validation.accepted.len(), 1);
        assert_eq!(validation.accepted[0].finding.claim, "one");
        assert!(
            validation.rejected[0]
                .reason
                .contains("duplicate finding id")
        );

        // Empty claim is a shape error.
        let mut empty = finding("f-2", ImpactClass::Correctness, Severity::Info, "");
        empty.evidence = vec![evidence("src/a.rs", "x")];
        let validation = validate_report(&context, "quality", &report("quality", vec![empty]));
        assert!(validation.accepted.is_empty());
        assert!(validation.rejected[0].reason.contains("`claim`"));

        // Oversized title is a shape error.
        let mut big = finding("f-3", ImpactClass::Correctness, Severity::Info, "c");
        big.title = "t".repeat(MAX_TITLE_BYTES + 1);
        let validation = validate_report(&context, "quality", &report("quality", vec![big]));
        assert!(validation.rejected[0].reason.contains("`title` exceeds"));

        // Empty expected identity rejects the report.
        let validation = validate_report(
            &context,
            "",
            &report(
                "quality",
                vec![finding(
                    "f-4",
                    ImpactClass::Correctness,
                    Severity::Info,
                    "c",
                )],
            ),
        );
        assert!(validation.accepted.is_empty());
        assert!(
            validation.rejected[0]
                .reason
                .contains("expected reviewer identity is empty")
        );
    }

    #[test]
    fn severity_clamp_and_evidence_requirement() {
        let repo = setup("val-severity");
        let records = BTreeSet::new();
        let context = repo.context(None, &records);

        // Style blocker clamps down to low.
        let style = finding("f-1", ImpactClass::Style, Severity::Blocker, "style issue");
        let validation = validate_report(&context, "quality", &report("quality", vec![style]));
        assert_eq!(validation.accepted[0].severity(), Severity::Low);

        // A correctness blocker with valid evidence survives at blocker.
        let correct = finding(
            "f-2",
            ImpactClass::Correctness,
            Severity::Blocker,
            "real bug",
        );
        let validation = validate_report(&context, "quality", &report("quality", vec![correct]));
        assert_eq!(validation.accepted[0].severity(), Severity::Blocker);

        // A high finding without evidence is excluded entirely.
        let mut unevidenced = finding(
            "f-3",
            ImpactClass::Correctness,
            Severity::High,
            "no evidence",
        );
        unevidenced.evidence = vec![];
        let validation =
            validate_report(&context, "quality", &report("quality", vec![unevidenced]));
        assert!(validation.accepted.is_empty());
        assert!(
            validation.rejected[0]
                .reason
                .contains("no concrete valid evidence")
        );

        // High finding whose only evidence is invalid is also excluded.
        let mut bad_evidence = finding("f-4", ImpactClass::Security, Severity::High, "bad path");
        bad_evidence.evidence = vec![evidence("src/missing.rs", "nothing")];
        let validation =
            validate_report(&context, "quality", &report("quality", vec![bad_evidence]));
        assert!(validation.accepted.is_empty());
        assert!(
            validation
                .rejected
                .iter()
                .any(|r| r.reason.contains("not part of the target snapshot"))
        );
        assert!(
            validation
                .rejected
                .iter()
                .any(|r| r.reason.contains("no concrete valid evidence"))
        );

        // Clamped-below-high severity drops the evidence requirement.
        let style_high = finding("f-5", ImpactClass::Style, Severity::High, "formatting");
        let validation = validate_report(&context, "quality", &report("quality", vec![style_high]));
        assert_eq!(validation.accepted[0].severity(), Severity::Low);
    }

    #[test]
    fn evidence_snapshot_membership_and_keys() {
        let repo = setup("val-evidence");
        let records = BTreeSet::new();
        let context = repo.context(None, &records);

        let bytes = std::fs::read(repo.ctx.worktree_root.join("src/a.rs")).unwrap();
        let good_key = working_tree_key(&bytes).as_str();
        let git_key = format!("git:{}", git_blob_oid(&bytes, ObjectFormat::Sha1));

        // Valid sha256 content key.
        let mut ok = finding("f-1", ImpactClass::Correctness, Severity::Info, "c");
        ok.evidence = vec![EvidenceRef {
            content_key: Some(good_key),
            ..evidence("src/a.rs", "fn a()")
        }];
        let validation = validate_report(&context, "quality", &report("quality", vec![ok]));
        assert_eq!(validation.accepted.len(), 1, "{:?}", validation.rejected);

        // Valid git: content key (the blob the exact bytes hash to).
        let mut ok_git = finding("f-2", ImpactClass::Correctness, Severity::Info, "c");
        ok_git.evidence = vec![EvidenceRef {
            content_key: Some(git_key),
            ..evidence("src/a.rs", "fn a()")
        }];
        let validation = validate_report(&context, "quality", &report("quality", vec![ok_git]));
        assert_eq!(validation.accepted.len(), 1, "{:?}", validation.rejected);

        // A content key of different content is rejected; an info-level
        // finding survives (it needs no evidence) with the entry dropped.
        let mut stale = finding("f-3", ImpactClass::Correctness, Severity::Info, "c");
        stale.evidence = vec![EvidenceRef {
            content_key: Some(
                "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                    .to_string(),
            ),
            ..evidence("src/a.rs", "fn a()")
        }];
        let validation = validate_report(&context, "quality", &report("quality", vec![stale]));
        assert_eq!(validation.accepted.len(), 1, "{:?}", validation.rejected);
        assert!(validation.accepted[0].finding.evidence.is_empty());
        assert!(
            validation.rejected[0]
                .reason
                .contains("does not match the target snapshot")
        );

        // The same rejected evidence excludes a high-severity finding.
        let mut stale_high = finding("f-3b", ImpactClass::Correctness, Severity::High, "c");
        stale_high.evidence = vec![EvidenceRef {
            content_key: Some(
                "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                    .to_string(),
            ),
            ..evidence("src/a.rs", "fn a()")
        }];
        let validation = validate_report(&context, "quality", &report("quality", vec![stale_high]));
        assert!(validation.accepted.is_empty());
        assert!(
            validation
                .rejected
                .iter()
                .any(|r| r.reason.contains("no concrete valid evidence"))
        );

        // Line ranges beyond the file are rejected (file has 3 lines).
        let mut beyond = finding("f-4", ImpactClass::Correctness, Severity::Info, "c");
        beyond.evidence = vec![EvidenceRef {
            line_start: Some(2),
            line_end: Some(9),
            ..evidence("src/a.rs", "fn a()")
        }];
        let validation = validate_report(&context, "quality", &report("quality", vec![beyond]));
        assert!(
            validation.rejected[0]
                .reason
                .contains("exceeds the 3 line(s)")
        );

        // Inverted ranges are rejected.
        let mut inverted = finding("f-5", ImpactClass::Correctness, Severity::Info, "c");
        inverted.evidence = vec![EvidenceRef {
            line_start: Some(3),
            line_end: Some(2),
            ..evidence("src/a.rs", "fn a()")
        }];
        let validation = validate_report(&context, "quality", &report("quality", vec![inverted]));
        assert!(
            validation.rejected[0]
                .reason
                .contains("line_start <= line_end")
        );

        // line_end without line_start is rejected.
        let mut dangled = finding("f-6", ImpactClass::Correctness, Severity::Info, "c");
        dangled.evidence = vec![EvidenceRef {
            line_end: Some(2),
            ..evidence("src/a.rs", "fn a()")
        }];
        let validation = validate_report(&context, "quality", &report("quality", vec![dangled]));
        assert!(
            validation.rejected[0]
                .reason
                .contains("`line_end` requires `line_start`")
        );
    }

    #[test]
    fn evidence_kind_specific_rules() {
        let repo = setup("val-kinds");
        let changed: BTreeSet<String> = ["src/a.rs".to_string(), "deleted/old.rs".to_string()]
            .into_iter()
            .collect();
        let records: BTreeSet<String> = ["01REC".to_string()].into_iter().collect();
        let context = repo.context(Some(&changed), &records);

        // Diff evidence must name a changed path (including deletions,
        // which are absent from the target snapshot).
        let mut diff_ok = finding("f-1", ImpactClass::Correctness, Severity::Info, "c");
        diff_ok.evidence = vec![EvidenceRef {
            kind: EvidenceKind::Diff,
            path: Some("src/a.rs".to_string()),
            ..evidence("src/a.rs", "+ fn a()")
        }];
        let validation = validate_report(&context, "quality", &report("quality", vec![diff_ok]));
        assert_eq!(validation.accepted.len(), 1, "{:?}", validation.rejected);

        let mut diff_bad = finding("f-2", ImpactClass::Correctness, Severity::Info, "c");
        diff_bad.evidence = vec![EvidenceRef {
            kind: EvidenceKind::Diff,
            path: Some("src/unchanged.rs".to_string()),
            ..evidence("src/unchanged.rs", "+ x")
        }];
        let validation = validate_report(&context, "quality", &report("quality", vec![diff_bad]));
        assert!(
            validation.rejected[0]
                .reason
                .contains("not part of the review scope")
        );

        // Diff evidence without a path is rejected.
        let mut diff_nopath = finding("f-3", ImpactClass::Correctness, Severity::Info, "c");
        diff_nopath.evidence = vec![EvidenceRef {
            kind: EvidenceKind::Diff,
            path: None,
            ..evidence("x", "+ x")
        }];
        let validation =
            validate_report(&context, "quality", &report("quality", vec![diff_nopath]));
        assert!(validation.rejected[0].reason.contains("requires `path`"));

        // Plan evidence is structural: bounded non-empty excerpt suffices.
        let mut plan = finding("f-4", ImpactClass::Correctness, Severity::Info, "c");
        plan.evidence = vec![EvidenceRef {
            kind: EvidenceKind::Plan,
            plan_section: Some("rollout".to_string()),
            ..evidence("plan.md", "Roll out behind a flag.")
        }];
        let validation = validate_report(&context, "quality", &report("quality", vec![plan]));
        assert_eq!(validation.accepted.len(), 1, "{:?}", validation.rejected);

        // Project-fact evidence must reference an existing record.
        let mut fact_ok = finding("f-5", ImpactClass::Correctness, Severity::Info, "c");
        fact_ok.evidence = vec![EvidenceRef {
            kind: EvidenceKind::ProjectFact,
            record_id: Some("01REC".to_string()),
            ..evidence("record", "The queue is lock-free.")
        }];
        let validation = validate_report(&context, "quality", &report("quality", vec![fact_ok]));
        assert_eq!(validation.accepted.len(), 1, "{:?}", validation.rejected);

        let mut fact_bad = finding("f-6", ImpactClass::Correctness, Severity::Info, "c");
        fact_bad.evidence = vec![EvidenceRef {
            kind: EvidenceKind::ProjectFact,
            record_id: Some("01MISSING".to_string()),
            ..evidence("record", "Hallucinated fact.")
        }];
        let validation = validate_report(&context, "quality", &report("quality", vec![fact_bad]));
        assert!(validation.rejected[0].reason.contains("does not exist"));
    }

    #[test]
    fn fingerprints_are_deterministic_and_normalizing() {
        let a = finding(
            "f-1",
            ImpactClass::Correctness,
            Severity::Info,
            "The   Queue Drops  Items ",
        );
        let mut b = finding(
            "f-9",
            ImpactClass::Correctness,
            Severity::High,
            "the queue drops items",
        );
        b.evidence = a.evidence.clone();

        let fa = finding_fingerprint(&a);
        let fb = finding_fingerprint(&b);
        assert_eq!(fa, fb, "normalization must ignore id, severity, and case");
        assert_eq!(fa.len(), 64);

        // Different claims differ.
        let mut c = finding(
            "f-1",
            ImpactClass::Correctness,
            Severity::Info,
            "completely different claim",
        );
        c.evidence = a.evidence.clone();
        assert_ne!(finding_fingerprint(&c), fa);

        // Different primary evidence differs.
        let mut d = finding(
            "f-1",
            ImpactClass::Correctness,
            Severity::Info,
            "the queue drops items",
        );
        d.evidence = vec![evidence("src/b.rs", "fn b()")];
        assert_ne!(finding_fingerprint(&d), fa);

        // Different impact classes differ.
        let mut e = finding(
            "f-1",
            ImpactClass::Security,
            Severity::Info,
            "the queue drops items",
        );
        e.evidence = a.evidence.clone();
        assert_ne!(finding_fingerprint(&e), fa);
    }

    /// A validated finding built directly (validation itself is covered
    /// by the report tests above).
    fn validated(reviewer: &str, id: &str, claim: &str, path: &str) -> ValidatedFinding {
        let mut f = finding(id, ImpactClass::Correctness, Severity::Info, claim);
        f.evidence = vec![evidence(path, "fn")];
        let fingerprint = finding_fingerprint(&f);
        ValidatedFinding {
            reviewer_id: reviewer.to_string(),
            fingerprint,
            finding: f,
        }
    }

    #[test]
    fn obvious_grouping_and_equivalence_candidates() {
        // Two reviewers with identical (normalized) claims at the same
        // location: one obvious group, no equivalence candidate.
        let findings = vec![
            validated("quality", "q-1", "queue drops items", "src/a.rs"),
            validated("concurrency", "c-1", "Queue drops  items", "src/a.rs"),
        ];
        let groups = group_findings(&findings);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].members.len(), 2);
        assert!(groups[0].cross_validated());
        assert!(equivalence_candidates(&groups).is_empty());

        // Same reviewer, different claims: two groups, but no
        // cross-reviewer pair, so no equivalence candidate.
        let findings = vec![
            validated("quality", "q-1", "queue drops items", "src/a.rs"),
            validated("quality", "q-2", "queue also leaks", "src/a.rs"),
        ];
        let groups = group_findings(&findings);
        assert_eq!(groups.len(), 2);
        assert!(equivalence_candidates(&groups).is_empty());

        // Two reviewers, same path, different claims: one candidate pair.
        let findings = vec![
            validated("quality", "q-1", "queue drops items", "src/a.rs"),
            validated("concurrency", "c-1", "queue leaks items", "src/a.rs"),
            validated("testing", "t-1", "untested path", "src/b.rs"),
        ];
        let groups = group_findings(&findings);
        assert_eq!(groups.len(), 3);
        assert_eq!(equivalence_candidates(&groups), vec![(0, 1)]);

        // Different impact classes never pair.
        let mut security_f = finding(
            "s-1",
            ImpactClass::Security,
            Severity::Info,
            "queue drops items",
        );
        security_f.evidence = vec![evidence("src/a.rs", "fn")];
        let findings = vec![
            validated("quality", "q-1", "queue drops items", "src/a.rs"),
            ValidatedFinding {
                reviewer_id: "security".to_string(),
                fingerprint: finding_fingerprint(&security_f),
                finding: security_f,
            },
        ];
        let groups = group_findings(&findings);
        assert_eq!(groups.len(), 2);
        assert!(equivalence_candidates(&groups).is_empty());
    }

    #[test]
    fn apply_equivalence_merges_only_same() {
        let findings = vec![
            validated("quality", "q-1", "claim a", "src/a.rs"),
            validated("concurrency", "c-1", "claim b", "src/a.rs"),
            validated("testing", "t-1", "claim c", "src/a.rs"),
        ];
        let groups = group_findings(&findings);

        let outcome = |verdict, check: &str| FindingEquivalenceOutcome {
            check_id: check.to_string(),
            verdict,
            error: None,
        };

        // same merges transitively: (0,1) and (1,2) -> one group.
        let pairs = vec![(0usize, 1usize), (1usize, 2usize)];
        let outcomes = vec![
            outcome(EquivalenceVerdict::Same, "k1"),
            outcome(EquivalenceVerdict::Same, "k2"),
        ];
        let mut diagnostics = Vec::new();
        let merged = apply_equivalence(groups.clone(), &pairs, &outcomes, &mut diagnostics);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].members.len(), 3);
        assert_eq!(merged[0].key, groups[0].key);
        assert!(merged[0].cross_validated());
        assert!(diagnostics.is_empty());

        // different and uncertain never merge.
        let outcomes = vec![
            outcome(EquivalenceVerdict::Different, "k3"),
            outcome(EquivalenceVerdict::Uncertain, "k4"),
        ];
        let merged = apply_equivalence(groups.clone(), &pairs, &outcomes, &mut diagnostics);
        assert_eq!(merged.len(), 3);
        assert!(diagnostics.is_empty());

        // Errored checks (forced uncertain) record diagnostics.
        let outcomes = vec![FindingEquivalenceOutcome {
            check_id: "k5".to_string(),
            verdict: EquivalenceVerdict::Uncertain,
            error: Some(HostErrorInfo {
                code: "session_timeout".to_string(),
                message: "timed out".to_string(),
            }),
        }];
        let merged = apply_equivalence(groups, &[(0, 1)], &outcomes, &mut diagnostics);
        assert_eq!(merged.len(), 3);
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("session_timeout"));
    }

    #[test]
    fn group_accessors_follow_member_order_and_policy() {
        // Members with different severities have different fingerprints,
        // so group_findings keeps them separate; a group like this arises
        // only via a same-equivalence merge.
        let low = validated_with_severity("quality", "q-1", Severity::Medium);
        let high = validated_with_severity("concurrency", "c-1", Severity::High);
        let group = FindingGroup {
            key: low.fingerprint.clone(),
            members: vec![low, high],
        };
        assert_eq!(group.max_severity(), Severity::High);
        assert_eq!(group.top_member().qualified_id(), "concurrency/c-1");
        assert_eq!(group.reviewer_ids(), vec!["concurrency", "quality"]);
        assert!(group.cross_validated());
    }

    fn validated_with_severity(reviewer: &str, id: &str, severity: Severity) -> ValidatedFinding {
        let mut f = finding(id, ImpactClass::Correctness, severity, "shared claim");
        f.evidence = vec![evidence("src/a.rs", "fn")];
        let fingerprint = finding_fingerprint(&f);
        ValidatedFinding {
            reviewer_id: reviewer.to_string(),
            fingerprint,
            finding: f,
        }
    }
}
