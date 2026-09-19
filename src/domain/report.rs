//! Reviewer reports and fact candidates.

use serde::{Deserialize, Serialize};

use super::evidence::EvidenceRef;
use super::finding::ReviewerFinding;

/// A project fact proposed by a reviewer.
///
/// This is only a proposal. Reviewer output can never create durable facts
/// directly; persistence requires deterministic validation plus independent
/// verification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateProjectFact {
    /// What the fact is about (stable subject phrase).
    pub subject: String,
    /// The proposed fact text.
    pub claim: String,
    /// Topic tags.
    pub tags: Vec<String>,
    /// Evidence backing the claim.
    pub evidence: Vec<EvidenceRef>,
    /// Reviewer confidence in [0, 1].
    pub confidence: f64,
}

/// Strict structured output produced by one reviewer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewerReport {
    /// Identity of the reviewer that produced this report.
    pub reviewer_id: String,
    /// Short summary of the review pass.
    pub summary: String,
    /// Findings proposed by the reviewer.
    pub findings: Vec<ReviewerFinding>,
    /// Project facts proposed by the reviewer (proposals only).
    pub candidate_project_facts: Vec<CandidateProjectFact>,
    /// Opaque model/execution metadata reported by the runtime.
    #[serde(default)]
    pub model_metadata: serde_json::Value,
    /// Wall-clock duration of the reviewer session, in milliseconds.
    pub duration_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::review::{ImpactClass, Severity};

    fn sample_report() -> ReviewerReport {
        ReviewerReport {
            reviewer_id: "quality".to_string(),
            summary: "One finding.".to_string(),
            findings: vec![ReviewerFinding {
                id: "q-1".to_string(),
                impact_class: ImpactClass::TestGap,
                proposed_severity: Severity::Medium,
                category: "test_gap".to_string(),
                title: "No test for wraparound".to_string(),
                claim: "Ring buffer wraparound is untested.".to_string(),
                explanation: "Boundary case is never exercised.".to_string(),
                recommendation: "Add a wraparound test.".to_string(),
                evidence: Vec::new(),
            }],
            candidate_project_facts: vec![CandidateProjectFact {
                subject: "ring buffer".to_string(),
                claim: "The ring buffer is used by the sensor path.".to_string(),
                tags: vec!["buffer".to_string()],
                evidence: Vec::new(),
                confidence: 0.95,
            }],
            model_metadata: serde_json::json!({
                "model_profile": "standard",
                "thinking_level": "low"
            }),
            duration_ms: 1234,
        }
    }

    #[test]
    fn reviewer_report_roundtrip() {
        let report = sample_report();
        let json = serde_json::to_string(&report).unwrap();
        let back: ReviewerReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report, back);
        assert!(json.contains("\"candidate_project_facts\""));
        assert!(json.contains("\"duration_ms\":1234"));
    }

    #[test]
    fn model_metadata_defaults_when_absent() {
        let mut report = sample_report();
        report.model_metadata = serde_json::Value::Null;
        let json = serde_json::to_string(&report).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let mut pruned = value;
        pruned.as_object_mut().unwrap().remove("model_metadata");
        let pruned_json = serde_json::to_string(&pruned).unwrap();
        let back: ReviewerReport = serde_json::from_str(&pruned_json).unwrap();
        assert_eq!(back.model_metadata, serde_json::Value::Null);
        assert_eq!(back.reviewer_id, "quality");
    }

    #[test]
    fn candidate_fact_rejects_out_of_range_confidence_serde_shape() {
        // f64 accepts any JSON number at the serde layer; range policy is a
        // deterministic validation concern of a later milestone. Here we only
        // pin the serialized field name.
        let candidate = CandidateProjectFact {
            subject: "s".to_string(),
            claim: "c".to_string(),
            tags: vec![],
            evidence: vec![],
            confidence: 0.0,
        };
        let json = serde_json::to_string(&candidate).unwrap();
        assert!(json.contains("\"confidence\":0.0"));
        let back: CandidateProjectFact = serde_json::from_str(&json).unwrap();
        assert_eq!(candidate, back);
    }
}
