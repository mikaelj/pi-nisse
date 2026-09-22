//! Investigation review domain types (PLAN_INVESTIGATION.md).
//!
//! `review_investigation(task, result)` evaluates whether an
//! investigation's claims and conclusions hold up. It produces
//! [`InvestigationResult`] — per-claim verdicts with confidence tiers and
//! provenance, plus coverage gaps — instead of the finding-shaped
//! [`crate::domain::review::ReviewResult`] used by plan/code reviews.
//! Verdicts are never mapped onto severities.
//!
//! The investigation result is UNTRUSTED input: nothing here inherits the
//! investigator's claimed confidence. Deterministic merging (Step 5 of the
//! implementation plan) is most-skeptical-wins with explicit disagreement
//! notes; synthesis only wordsmiths.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::evidence::EvidenceRef;
use super::ids::is_valid_ulid;

/// Schema version of the investigation result payloads.
pub const INVESTIGATION_SCHEMA_VERSION: u32 = 1;

/// Maximum size of one resolved investigation input (task or result),
/// matching the plan-input bound.
pub const MAX_INVESTIGATION_INPUT_BYTES: usize = 256 * 1024;

/// Minimum normalized length an `Investigation`-kind evidence excerpt must
/// have to count as an input-span anchor (shorter excerpts are trivially
/// satisfiable against any input).
pub const MIN_INPUT_SPAN_EXCERPT_CHARS: usize = 24;

/// One investigation review verdict for a claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvestigationVerdict {
    /// The claim holds as stated.
    Supported,
    /// The claim holds only in a weaker or narrower form.
    PartiallySupported,
    /// The claim is refuted by the evidence.
    Contradicted,
    /// The evidence offered does not establish the claim.
    InsufficientlySupported,
    /// The claim does not apply to the target system/version/context.
    NotApplicable,
    /// The claim is outside the reviewed task's scope.
    OutsideScope,
}

impl InvestigationVerdict {
    /// Skepticism precedence for deterministic merging: higher wins when
    /// reviewers disagree (most skeptical wins). Relevance verdicts
    /// (`NotApplicable`, `OutsideScope`) are excluded — they only stand
    /// when unanimous (handled by the merge, not by this ordering).
    pub fn skepticism(self) -> u8 {
        match self {
            InvestigationVerdict::Contradicted => 5,
            InvestigationVerdict::InsufficientlySupported => 4,
            InvestigationVerdict::PartiallySupported => 3,
            InvestigationVerdict::Supported => 2,
            InvestigationVerdict::NotApplicable | InvestigationVerdict::OutsideScope => 1,
        }
    }

    /// Whether this verdict questions the claim's relevance rather than
    /// its substance.
    pub fn is_relevance(self) -> bool {
        matches!(
            self,
            InvestigationVerdict::NotApplicable | InvestigationVerdict::OutsideScope
        )
    }
}

/// How well the surviving evidence actually establishes a claim.
///
/// Variants are declared in ASCENDING support: the derived ordering has
/// `Unsupported` as the least and `Demonstrated` as the greatest, so
/// `min` selects the least supportive tier (deterministic merging).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceTier {
    /// Nothing supports the claim.
    Unsupported,
    /// The evidence shows the claim is possible, nothing more.
    MerelyPossible,
    /// Consistent with the evidence but not established by it.
    Plausible,
    /// Strongly supported, with minor inferential steps.
    StronglySupported,
    /// Directly demonstrated by the evidence.
    Demonstrated,
}

/// Clamp a verdict/confidence pair into logical consistency (merge
/// normalization): relevance verdicts cap confidence at `Plausible`, and
/// an affirmative verdict cannot outrun its evidence.
pub fn clamp_verdict_confidence(
    verdict: InvestigationVerdict,
    confidence: ConfidenceTier,
) -> ConfidenceTier {
    use ConfidenceTier as Tier;
    match verdict {
        InvestigationVerdict::NotApplicable | InvestigationVerdict::OutsideScope => {
            confidence.min(Tier::Plausible)
        }
        InvestigationVerdict::Supported => confidence.min(Tier::StronglySupported),
        _ => confidence,
    }
}

/// Which investigation input a reference points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskOrResult {
    /// The task input.
    Task,
    /// The result input under review.
    Result,
}

/// One assessed claim of the investigation result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaimAssessment {
    /// Payload schema version; must be `1`.
    pub schema_version: u32,
    /// Identifier of this assessment (ULID).
    pub id: String,
    /// The claim under assessment (compressed quote of the result text).
    pub claim: String,
    /// The verdict.
    pub verdict: InvestigationVerdict,
    /// How well the surviving evidence establishes the claim.
    pub confidence: ConfidenceTier,
    /// What is overstated in the original conclusion and why, if anything.
    pub overstated: Option<String>,
    /// Why the claim matters (or not) for the original task.
    pub task_relevance: String,
    /// Evidence backing the assessment.
    pub evidence: Vec<EvidenceRef>,
    /// The reasoning connecting evidence to verdict.
    pub reasoning: String,
    /// IDs of the source per-reviewer assessments merged into this one
    /// (synthesis provenance; never empty after validation).
    pub source_claim_ids: Vec<String>,
    /// Reviewers that assessed the claim.
    pub reviewer_ids: Vec<String>,
    /// Whether equivalent assessments arrived from >= 2 independent
    /// reviewers.
    pub cross_validated: bool,
}

/// One important area the investigation should reasonably have examined
/// but missed or materially under-investigated. A coverage gap means
/// incompleteness — it is never a newly proven finding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoverageGap {
    /// Payload schema version; must be `1`.
    pub schema_version: u32,
    /// Identifier of this gap (ULID).
    pub id: String,
    /// The under-investigated area.
    pub area: String,
    /// Why this area was reasonably in scope for the task (citing the
    /// task text).
    pub why_expected: String,
    /// Evidence for the gap assessment.
    pub evidence: Vec<EvidenceRef>,
    /// How important the gap is.
    pub importance: GapImportance,
    /// Reviewers that reported the gap.
    pub reviewer_ids: Vec<String>,
}

/// Importance of a coverage gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GapImportance {
    /// High.
    High,
    /// Medium.
    Medium,
    /// Low.
    Low,
}

/// The result of one investigation review.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvestigationResult {
    /// Payload schema version; must be `1`.
    pub schema_version: u32,
    /// Identifier of this review (ULID).
    pub review_id: String,
    /// Always `Investigation`.
    pub kind: crate::domain::review::ReviewKind,
    /// Creation time (UTC RFC3339).
    pub created_at: String,
    /// Snapshot every part of this review resolved against.
    pub snapshot: crate::domain::snapshot::SnapshotDescriptor,
    /// Synthesized review summary.
    pub summary: String,
    /// Assessments of the investigation's claims.
    pub claims: Vec<ClaimAssessment>,
    /// Important areas the investigation missed or under-investigated.
    pub coverage_gaps: Vec<CoverageGap>,
}

/// One resolved investigation input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputValue {
    /// The input content (never blank; bounded by
    /// [`MAX_INVESTIGATION_INPUT_BYTES`]).
    pub text: String,
    /// Where the content came from.
    pub origin: InputOrigin,
}

/// Where an [`InputValue`] came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputOrigin {
    /// Literal text supplied on the command line / over the API.
    Literal,
    /// A file reference (explicit `@file` or an auto-detected path).
    File { path: PathBuf },
}

/// Typed investigation-input failures.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum InvestigationInputError {
    /// An explicit `@file` reference does not name a readable file.
    #[error("@file reference `{spec}` does not name a readable file")]
    MissingFile { spec: String },
    /// The input resolved to blank content.
    #[error("the {which} input is blank")]
    Blank { which: &'static str },
    /// The input is not valid UTF-8.
    #[error("@file reference `{spec}` is not valid UTF-8")]
    NotUtf8 { spec: String },
    /// The input exceeds the size bound.
    #[error("the {which} input exceeds {MAX_INVESTIGATION_INPUT_BYTES} bytes")]
    TooLarge { which: &'static str },
}

/// Resolve one `--task`/`--result` value.
///
/// Rules (PLAN_INVESTIGATION.md PURPOSE): a leading `@` explicitly means
/// file (relative paths resolve against the worktree root when they exist
/// there, else against `cwd`; missing files are typed errors). Without
/// `@`, a value naming an existing file (worktree-root-relative,
/// cwd-relative, or absolute) is read as a file; anything else is literal
/// text. Blank content and oversized inputs are typed errors.
pub fn resolve_input_value(
    spec: &str,
    which: &'static str,
    worktree_root: Option<&Path>,
    cwd: &Path,
) -> Result<InputValue, InvestigationInputError> {
    let read_file = |path: &Path| -> Result<InputValue, InvestigationInputError> {
        let bytes = std::fs::read(path).map_err(|_| InvestigationInputError::MissingFile {
            spec: spec.to_string(),
        })?;
        if bytes.len() > MAX_INVESTIGATION_INPUT_BYTES {
            return Err(InvestigationInputError::TooLarge { which });
        }
        let text = String::from_utf8(bytes).map_err(|_| InvestigationInputError::NotUtf8 {
            spec: spec.to_string(),
        })?;
        Ok(InputValue {
            text,
            origin: InputOrigin::File {
                path: path.to_path_buf(),
            },
        })
    };

    let value = if let Some(path_spec) = spec.strip_prefix('@') {
        // Explicit file: worktree-root-relative, then cwd-relative, then
        // absolute/literal-path as given.
        let path = worktree_root
            .map(|root| root.join(path_spec))
            .filter(|candidate| candidate.is_file())
            .unwrap_or_else(|| cwd.join(path_spec));
        read_file(&path)?
    } else if let Some(existing) = worktree_root
        .map(|root| root.join(spec))
        .filter(|candidate| candidate.is_file())
        .or_else(|| {
            let candidate = cwd.join(spec);
            candidate.is_file().then_some(candidate)
        })
        .or_else(|| Path::new(spec).is_file().then(|| PathBuf::from(spec)))
    {
        read_file(&existing)?
    } else {
        InputValue {
            text: spec.to_string(),
            origin: InputOrigin::Literal,
        }
    };

    if value.text.trim().is_empty() {
        return Err(InvestigationInputError::Blank { which });
    }
    if value.text.len() > MAX_INVESTIGATION_INPUT_BYTES {
        return Err(InvestigationInputError::TooLarge { which });
    }
    Ok(value)
}

/// Validate one claim assessment's structural invariants (identity,
/// provenance shape). Verdict/evidence validation lives in the pipeline
/// (it needs the resolved inputs and snapshot).
pub fn validate_claim_shape(claim: &ClaimAssessment) -> Result<(), String> {
    if claim.schema_version != INVESTIGATION_SCHEMA_VERSION {
        return Err(format!(
            "claim {} has schema_version {} (expected {INVESTIGATION_SCHEMA_VERSION})",
            claim.id, claim.schema_version
        ));
    }
    if !is_valid_ulid(&claim.id) {
        return Err(format!("claim id `{}` is not a ULID", claim.id));
    }
    if claim.claim.trim().is_empty() {
        return Err(format!("claim {} has no claim text", claim.id));
    }
    if claim.task_relevance.trim().is_empty() {
        return Err(format!("claim {} has no task relevance", claim.id));
    }
    if claim.reasoning.trim().is_empty() {
        return Err(format!("claim {} has no reasoning", claim.id));
    }
    if claim.reviewer_ids.is_empty() {
        return Err(format!("claim {} has no reviewer provenance", claim.id));
    }
    Ok(())
}

/// Validate one coverage gap's structural invariants.
pub fn validate_gap_shape(gap: &CoverageGap) -> Result<(), String> {
    if gap.schema_version != INVESTIGATION_SCHEMA_VERSION {
        return Err(format!(
            "coverage gap {} has schema_version {} (expected {INVESTIGATION_SCHEMA_VERSION})",
            gap.id, gap.schema_version
        ));
    }
    if !is_valid_ulid(&gap.id) {
        return Err(format!("coverage gap id `{}` is not a ULID", gap.id));
    }
    if gap.area.trim().is_empty() {
        return Err(format!("coverage gap {} has no area", gap.id));
    }
    if gap.why_expected.trim().is_empty() {
        return Err(format!("coverage gap {} has no why_expected", gap.id));
    }
    if gap.reviewer_ids.is_empty() {
        return Err(format!(
            "coverage gap {} has no reviewer provenance",
            gap.id
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_and_confidence_serde_is_snake_case() {
        assert_eq!(
            serde_json::to_string(&InvestigationVerdict::InsufficientlySupported).unwrap(),
            "\"insufficiently_supported\""
        );
        assert_eq!(
            serde_json::to_string(&ConfidenceTier::StronglySupported).unwrap(),
            "\"strongly_supported\""
        );
        assert!(serde_json::from_str::<InvestigationVerdict>("\"supported\"").is_ok());
        assert!(serde_json::from_str::<InvestigationVerdict>("\"fine\"").is_err());
    }

    #[test]
    fn skepticism_ordering_is_most_critical_first() {
        let order = [
            InvestigationVerdict::Contradicted,
            InvestigationVerdict::InsufficientlySupported,
            InvestigationVerdict::PartiallySupported,
            InvestigationVerdict::Supported,
        ];
        for pair in order.windows(2) {
            assert!(
                pair[0].skepticism() > pair[1].skepticism(),
                "{pair:?} must be descending"
            );
        }
        assert!(InvestigationVerdict::NotApplicable.is_relevance());
        assert!(InvestigationVerdict::OutsideScope.is_relevance());
    }

    #[test]
    fn confidence_tiers_order_by_support() {
        assert!(ConfidenceTier::Demonstrated > ConfidenceTier::Plausible);
        assert!(ConfidenceTier::Plausible > ConfidenceTier::Unsupported);
        assert_eq!(
            ConfidenceTier::min(ConfidenceTier::Demonstrated, ConfidenceTier::MerelyPossible),
            ConfidenceTier::MerelyPossible
        );
    }

    #[test]
    fn verdict_confidence_pairs_clamp_into_consistency() {
        assert_eq!(
            clamp_verdict_confidence(
                InvestigationVerdict::NotApplicable,
                ConfidenceTier::Demonstrated
            ),
            ConfidenceTier::Plausible
        );
        assert_eq!(
            clamp_verdict_confidence(
                InvestigationVerdict::Supported,
                ConfidenceTier::Demonstrated
            ),
            ConfidenceTier::StronglySupported
        );
        assert_eq!(
            clamp_verdict_confidence(
                InvestigationVerdict::Contradicted,
                ConfidenceTier::Demonstrated
            ),
            ConfidenceTier::Demonstrated
        );
    }

    #[test]
    fn input_resolution_at_file_auto_file_and_literal() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let cwd = dir.path().join("elsewhere");
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(root.join("docs").join("task.md"), "the task text").unwrap();
        std::fs::write(cwd.join("result.txt"), "the result text").unwrap();

        // Explicit @file, worktree-root-relative.
        let value = resolve_input_value("@docs/task.md", "task", Some(&root), &cwd).unwrap();
        assert_eq!(value.text, "the task text");
        assert!(matches!(value.origin, InputOrigin::File { .. }));

        // Explicit @file, cwd-relative.
        let value = resolve_input_value("@result.txt", "result", Some(&root), &cwd).unwrap();
        assert_eq!(value.text, "the result text");

        // Auto-detected file (worktree-root-relative, no @).
        let value = resolve_input_value("docs/task.md", "task", Some(&root), &cwd).unwrap();
        assert!(matches!(value.origin, InputOrigin::File { .. }));

        // Literal: not a file anywhere.
        let value = resolve_input_value(
            "is the firewall rule actually applied?",
            "task",
            Some(&root),
            &cwd,
        )
        .unwrap();
        assert_eq!(value.text, "is the firewall rule actually applied?");
        assert_eq!(value.origin, InputOrigin::Literal);
    }

    #[test]
    fn input_resolution_rejects_missing_blank_and_oversized() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();

        match resolve_input_value("@missing.md", "task", Some(&root), dir.path()).unwrap_err() {
            InvestigationInputError::MissingFile { spec } => assert_eq!(spec, "@missing.md"),
            other => panic!("{other:?}"),
        }

        // Blank literal and blank file.
        assert!(matches!(
            resolve_input_value("   ", "result", Some(&root), dir.path()),
            Err(InvestigationInputError::Blank { which: "result" })
        ));
        std::fs::write(root.join("empty.md"), "  \n").unwrap();
        assert!(matches!(
            resolve_input_value("@empty.md", "task", Some(&root), dir.path()),
            Err(InvestigationInputError::Blank { which: "task" })
        ));

        // Oversized input.
        let big = "x".repeat(MAX_INVESTIGATION_INPUT_BYTES + 1);
        assert!(matches!(
            resolve_input_value(&big, "result", Some(&root), dir.path()),
            Err(InvestigationInputError::TooLarge { which: "result" })
        ));
    }
}
