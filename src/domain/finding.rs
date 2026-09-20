//! Reviewer findings and consolidated final findings.

use serde::{Deserialize, Serialize};

use super::evidence::EvidenceRef;
use super::review::{ImpactClass, Severity};

/// A single structured finding proposed by one reviewer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewerFinding {
    /// Reviewer-local finding identifier.
    pub id: String,
    /// Claimed impact class.
    pub impact_class: ImpactClass,
    /// Severity proposed by the reviewer; Rust applies severity policy.
    pub proposed_severity: Severity,
    /// Short machine-readable category.
    pub category: String,
    /// Short human title.
    pub title: String,
    /// One-sentence claim of what is wrong.
    pub claim: String,
    /// Why this is a problem.
    pub explanation: String,
    /// Suggested remediation.
    pub recommendation: String,
    /// Concrete evidence backing the finding.
    pub evidence: Vec<EvidenceRef>,
}

/// A consolidated finding in a final review result.
///
/// Final findings are synthesized only from validated reviewer findings;
/// every final finding preserves its source finding and reviewer provenance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FinalFinding {
    /// Final finding identifier (ULID).
    pub id: String,
    /// Impact class carried from source findings.
    pub impact_class: ImpactClass,
    /// Final severity; synthesis may lower but never raise it.
    pub severity: Severity,
    /// Short machine-readable category.
    pub category: String,
    /// Short human title.
    pub title: String,
    /// One-sentence claim of what is wrong.
    pub claim: String,
    /// Why this is a problem.
    pub explanation: String,
    /// Suggested remediation.
    pub recommendation: String,
    /// Combined evidence from source findings.
    pub evidence: Vec<EvidenceRef>,
    /// IDs of the validated reviewer findings this finding was built from.
    pub source_finding_ids: Vec<String>,
    /// IDs of the reviewers that produced the source findings.
    pub reviewer_ids: Vec<String>,
    /// Whether equivalent source findings came from at least two
    /// independent reviewers.
    pub cross_validated: bool,
}

/// Which stage of the consolidation pipeline produced a diagnostic
/// (plan section 38).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SynthesisStage {
    /// A narrow finding-equivalence check.
    Equivalence,
    /// The final finding synthesis session.
    Synthesis,
}

/// A retained diagnostic of the consolidation pipeline: why an outcome
/// fell back to deterministic behavior, why a synthesized entry was
/// rejected, or why an equivalence check failed. Diagnostics never
/// change review content; they make conservative degradation observable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynthesisDiagnostic {
    /// Pipeline stage that produced this diagnostic.
    pub stage: SynthesisStage,
    /// Human-readable explanation.
    pub message: String,
}

impl SynthesisDiagnostic {
    /// Build a diagnostic for `stage`.
    pub fn new(stage: SynthesisStage, message: impl Into<String>) -> Self {
        SynthesisDiagnostic {
            stage,
            message: message.into(),
        }
    }
}

/// The result of consolidating validated reviewer findings into final
/// findings (plan section 38).
///
/// Synthesis may only restate, clarify, combine, and lower severity of
/// existing validated findings; every [`FinalFinding`] preserves its
/// source provenance. Diagnostics record every conservative fallback.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SynthesisResult {
    /// Synthesized review summary.
    pub summary: String,
    /// Consolidated final findings.
    pub findings: Vec<FinalFinding>,
    /// Retained diagnostics of the consolidation pipeline.
    pub diagnostics: Vec<SynthesisDiagnostic>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_reviewer_finding() -> ReviewerFinding {
        ReviewerFinding {
            id: "q-1".to_string(),
            impact_class: ImpactClass::Correctness,
            proposed_severity: Severity::High,
            category: "logic".to_string(),
            title: "Off-by-one in ring buffer".to_string(),
            claim: "The ring buffer drops the last element.".to_string(),
            explanation: "Wraparound check uses > instead of >=.".to_string(),
            recommendation: "Use >= when comparing head and tail.".to_string(),
            evidence: vec![EvidenceRef {
                kind: crate::domain::evidence::EvidenceKind::Source,
                path: Some("src/buffer.rs".to_string()),
                symbol: Some("push".to_string()),
                line_start: Some(7),
                line_end: Some(9),
                content_key: None,
                plan_section: None,
                record_id: None,
                excerpt: "if self.head > self.tail {".to_string(),
            }],
        }
    }

    #[test]
    fn reviewer_finding_roundtrip() {
        let finding = sample_reviewer_finding();
        let json = serde_json::to_string(&finding).unwrap();
        let back: ReviewerFinding = serde_json::from_str(&json).unwrap();
        assert_eq!(finding, back);
        assert!(json.contains("\"proposed_severity\":\"high\""));
        assert!(json.contains("\"impact_class\":\"correctness\""));
    }

    #[test]
    fn reviewer_finding_requires_all_fields() {
        let finding = sample_reviewer_finding();
        let json = serde_json::to_string(&finding).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        for field in [
            "id",
            "impact_class",
            "proposed_severity",
            "category",
            "title",
            "claim",
            "explanation",
            "recommendation",
            "evidence",
        ] {
            let mut pruned = value.clone();
            assert!(pruned.as_object_mut().unwrap().remove(field).is_some());
            let pruned_json = serde_json::to_string(&pruned).unwrap();
            assert!(
                serde_json::from_str::<ReviewerFinding>(&pruned_json).is_err(),
                "expected rejection when `{field}` is missing"
            );
        }
    }

    #[test]
    fn final_finding_roundtrip_and_provenance_fields() {
        let finding = FinalFinding {
            id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string(),
            impact_class: ImpactClass::Security,
            severity: Severity::Medium,
            category: "auth".to_string(),
            title: "Weak token check".to_string(),
            claim: "Tokens are compared non-constantly.".to_string(),
            explanation: "Early exit leaks token bytes.".to_string(),
            recommendation: "Use a constant-time comparison.".to_string(),
            evidence: Vec::new(),
            source_finding_ids: vec!["q-1".to_string(), "s-2".to_string()],
            reviewer_ids: vec!["quality".to_string(), "security".to_string()],
            cross_validated: true,
        };
        let json = serde_json::to_string(&finding).unwrap();
        let back: FinalFinding = serde_json::from_str(&json).unwrap();
        assert_eq!(finding, back);
        assert!(json.contains("\"source_finding_ids\":[\"q-1\",\"s-2\"]"));
        assert!(json.contains("\"reviewer_ids\":[\"quality\",\"security\"]"));
        assert!(json.contains("\"cross_validated\":true"));
    }

    #[test]
    fn synthesis_result_roundtrip() {
        let result = SynthesisResult {
            summary: "One consolidated finding.".to_string(),
            findings: vec![FinalFinding {
                id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string(),
                impact_class: ImpactClass::Correctness,
                severity: Severity::Medium,
                category: "logic".to_string(),
                title: "Off-by-one".to_string(),
                claim: "Drops the last element.".to_string(),
                explanation: "Wraparound check is wrong.".to_string(),
                recommendation: "Use >=.".to_string(),
                evidence: Vec::new(),
                source_finding_ids: vec!["quality/q-1".to_string()],
                reviewer_ids: vec!["quality".to_string()],
                cross_validated: false,
            }],
            diagnostics: vec![SynthesisDiagnostic::new(
                SynthesisStage::Synthesis,
                "synthesis session failed; deterministic wording used".to_string(),
            )],
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: SynthesisResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, back);
        assert!(json.contains("\"stage\":\"synthesis\""));
        assert!(json.contains("\"diagnostics\""));
        let stage_json = serde_json::to_string(&SynthesisStage::Equivalence).unwrap();
        assert_eq!(stage_json, "\"equivalence\"");
        assert!(serde_json::from_str::<SynthesisStage>("\"merge\"").is_err());
    }
}
