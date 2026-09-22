//! Evidence references.
//!
//! Findings must point at concrete evidence: source code, diff content, plan
//! content, or project records. Structural validation of evidence belongs to
//! later milestones; this module defines the frozen shape.

use serde::{Deserialize, Serialize};

/// What kind of content an evidence reference points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    /// Source code in the target snapshot.
    Source,
    /// Diff content of the review input.
    Diff,
    /// Plan content (plan reviews only).
    Plan,
    /// A durable project record.
    ProjectFact,
    /// Investigation task/result input text (investigation reviews
    /// only; PLAN_INVESTIGATION.md).
    Investigation,
}

/// A reference to concrete evidence backing a finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRef {
    /// What kind of content this refers to.
    pub kind: EvidenceKind,
    /// Repository-relative path, when the evidence is path-bound.
    pub path: Option<String>,
    /// Optional symbol within the path.
    pub symbol: Option<String>,
    /// Optional first line (1-based, inclusive).
    pub line_start: Option<u32>,
    /// Optional last line (1-based, inclusive).
    pub line_end: Option<u32>,
    /// Optional content key (`git:<blob-oid>` or `sha256:<hash>`).
    pub content_key: Option<String>,
    /// Optional plan section identifier (plan evidence).
    pub plan_section: Option<String>,
    /// Optional durable record ID (project-fact evidence).
    pub record_id: Option<String>,
    /// Which investigation input this refers to (investigation
    /// evidence only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_part: Option<super::investigation::TaskOrResult>,
    /// Short excerpt of the referenced content.
    pub excerpt: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> EvidenceRef {
        EvidenceRef {
            kind: EvidenceKind::Source,
            path: Some("src/lib.rs".to_string()),
            symbol: Some("enqueue".to_string()),
            line_start: Some(42),
            line_end: Some(48),
            content_key: Some("git:0123456789abcdef0123456789abcdef01234567".to_string()),
            plan_section: None,
            record_id: None,
            input_part: None,
            excerpt: "self.head = (self.head + 1) % len;".to_string(),
        }
    }

    #[test]
    fn evidence_kind_roundtrip() {
        for (kind, name) in [
            (EvidenceKind::Source, "\"source\""),
            (EvidenceKind::Diff, "\"diff\""),
            (EvidenceKind::Plan, "\"plan\""),
            (EvidenceKind::ProjectFact, "\"project_fact\""),
        ] {
            assert_eq!(serde_json::to_string(&kind).unwrap(), name);
            let back: EvidenceKind = serde_json::from_str(name).unwrap();
            assert_eq!(kind, back);
        }
    }

    #[test]
    fn evidence_kind_rejects_unknown_variants() {
        assert!(serde_json::from_str::<EvidenceKind>("\"commit\"").is_err());
        assert!(serde_json::from_str::<EvidenceKind>("\"Source\"").is_err());
    }

    #[test]
    fn evidence_ref_roundtrip() {
        let evidence = sample();
        let json = serde_json::to_string(&evidence).unwrap();
        let back: EvidenceRef = serde_json::from_str(&json).unwrap();
        assert_eq!(evidence, back);
        assert!(json.contains("\"line_start\":42"));
        assert!(json.contains("\"content_key\""));
        assert!(json.contains("\"excerpt\""));
    }

    #[test]
    fn evidence_ref_minimal_roundtrip() {
        let evidence = EvidenceRef {
            kind: EvidenceKind::Plan,
            path: None,
            symbol: None,
            line_start: None,
            line_end: None,
            content_key: None,
            plan_section: Some("risk".to_string()),
            record_id: None,
            input_part: None,
            excerpt: "Roll out behind a flag.".to_string(),
        };
        let json = serde_json::to_string(&evidence).unwrap();
        let back: EvidenceRef = serde_json::from_str(&json).unwrap();
        assert_eq!(evidence, back);
    }
}
