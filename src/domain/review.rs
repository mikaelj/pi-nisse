//! Review kinds, severity, impact classes, and the review result.

use serde::{Deserialize, Serialize};

use super::finding::FinalFinding;
use super::snapshot::SnapshotDescriptor;

/// What is being reviewed.
///
/// Nisse Core reviews plans and code produced by others. It has no task
/// workflow modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReviewKind {
    Plan,
    Code,
    Investigation,
}

/// Finding severity.
///
/// Variants are declared in increasing severity; the derived `Ord`
/// implements that ordering (`Info` is the least, `Blocker` the greatest).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
    Blocker,
}

/// The kind of impact a finding claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImpactClass {
    Correctness,
    Security,
    DataLoss,
    Deadlock,
    Crash,
    Compatibility,
    Performance,
    TestGap,
    Maintainability,
    Style,
    Other,
}

impl ImpactClass {
    /// The serialized name (snake_case, matching the serde rename).
    pub fn as_str(self) -> &'static str {
        match self {
            ImpactClass::Correctness => "correctness",
            ImpactClass::Security => "security",
            ImpactClass::DataLoss => "data_loss",
            ImpactClass::Deadlock => "deadlock",
            ImpactClass::Crash => "crash",
            ImpactClass::Compatibility => "compatibility",
            ImpactClass::Performance => "performance",
            ImpactClass::TestGap => "test_gap",
            ImpactClass::Maintainability => "maintainability",
            ImpactClass::Style => "style",
            ImpactClass::Other => "other",
        }
    }
}

impl ImpactClass {
    /// The maximum severity this impact class may carry (plan section 14).
    ///
    /// `style` is capped at `low`; `maintainability` and `test_gap` at
    /// `medium`; `performance`, `compatibility`, and `other` at `high`.
    /// Only `correctness`, `security`, `data_loss`, `deadlock`, and
    /// `crash` may reach `blocker`, and even then only when concrete
    /// evidence supports the finding (enforced separately by the
    /// evidence requirement for high/blocker findings, plan section 15).
    pub fn max_severity(self) -> Severity {
        match self {
            ImpactClass::Style => Severity::Low,
            ImpactClass::Maintainability | ImpactClass::TestGap => Severity::Medium,
            ImpactClass::Performance | ImpactClass::Compatibility | ImpactClass::Other => {
                Severity::High
            }
            ImpactClass::Correctness
            | ImpactClass::Security
            | ImpactClass::DataLoss
            | ImpactClass::Deadlock
            | ImpactClass::Crash => Severity::Blocker,
        }
    }
}

/// The final result of a review.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewResult {
    /// Identifier of this review (ULID).
    pub review_id: String,
    /// What kind of review produced this result.
    pub kind: ReviewKind,
    /// Creation time (UTC RFC3339).
    pub created_at: String,
    /// Snapshot every part of this review resolved against.
    pub snapshot: SnapshotDescriptor,
    /// Synthesized review summary.
    pub summary: String,
    /// Validated, consolidated final findings.
    pub findings: Vec<FinalFinding>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::evidence::{EvidenceKind, EvidenceRef};

    fn sample_evidence() -> EvidenceRef {
        EvidenceRef {
            kind: EvidenceKind::Source,
            path: Some("src/main.rs".to_string()),
            symbol: Some("main".to_string()),
            line_start: Some(10),
            line_end: Some(12),
            content_key: Some("git:0123456789abcdef0123456789abcdef01234567".to_string()),
            plan_section: None,
            record_id: None,
            input_part: None,
            excerpt: "fn main() {".to_string(),
        }
    }

    fn sample_snapshot() -> SnapshotDescriptor {
        SnapshotDescriptor {
            mode: crate::domain::snapshot::SnapshotMode::Working,
            base_commit: Some("0123456789abcdef0123456789abcdef01234567".to_string()),
            base_tree: Some("fedcba9876543210fedcba9876543210fedcba98".to_string()),
            head_commit: None,
            head_tree: None,
        }
    }

    fn sample_finding() -> FinalFinding {
        FinalFinding {
            id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string(),
            impact_class: ImpactClass::Correctness,
            severity: Severity::High,
            category: "race".to_string(),
            title: "Unsynchronized queue access".to_string(),
            claim: "The queue is mutated from two contexts.".to_string(),
            explanation: "ISR writes while main loop reads.".to_string(),
            recommendation: "Guard access or use a message queue.".to_string(),
            evidence: vec![sample_evidence()],
            source_finding_ids: vec!["01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string()],
            reviewer_ids: vec!["quality".to_string(), "concurrency".to_string()],
            cross_validated: true,
        }
    }

    #[test]
    fn review_kind_roundtrip() {
        for kind in [ReviewKind::Plan, ReviewKind::Code] {
            let json = serde_json::to_string(&kind).unwrap();
            let back: ReviewKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, back);
        }
        assert_eq!(
            serde_json::to_string(&ReviewKind::Plan).unwrap(),
            "\"plan\""
        );
        assert_eq!(
            serde_json::to_string(&ReviewKind::Code).unwrap(),
            "\"code\""
        );
    }

    #[test]
    fn review_kind_rejects_unknown_variants() {
        assert!(serde_json::from_str::<ReviewKind>("\"build\"").is_err());
        assert!(serde_json::from_str::<ReviewKind>("\"PLAN\"").is_err());
    }

    #[test]
    fn severity_roundtrip_and_ordering() {
        assert_eq!(
            serde_json::to_string(&Severity::Blocker).unwrap(),
            "\"blocker\""
        );
        assert_eq!(serde_json::to_string(&Severity::Info).unwrap(), "\"info\"");
        for severity in [
            Severity::Blocker,
            Severity::High,
            Severity::Medium,
            Severity::Low,
            Severity::Info,
        ] {
            let json = serde_json::to_string(&severity).unwrap();
            let back: Severity = serde_json::from_str(&json).unwrap();
            assert_eq!(severity, back);
        }
        assert!(Severity::Blocker > Severity::High);
        assert!(Severity::High > Severity::Medium);
        assert!(Severity::Medium > Severity::Low);
        assert!(Severity::Low > Severity::Info);
    }

    #[test]
    fn severity_rejects_unknown_variants() {
        assert!(serde_json::from_str::<Severity>("\"critical\"").is_err());
        assert!(serde_json::from_str::<Severity>("\"BLOCKER\"").is_err());
    }

    #[test]
    fn impact_class_roundtrip() {
        for (class, name) in [
            (ImpactClass::Correctness, "\"correctness\""),
            (ImpactClass::Security, "\"security\""),
            (ImpactClass::DataLoss, "\"data_loss\""),
            (ImpactClass::Deadlock, "\"deadlock\""),
            (ImpactClass::Crash, "\"crash\""),
            (ImpactClass::Compatibility, "\"compatibility\""),
            (ImpactClass::Performance, "\"performance\""),
            (ImpactClass::TestGap, "\"test_gap\""),
            (ImpactClass::Maintainability, "\"maintainability\""),
            (ImpactClass::Style, "\"style\""),
            (ImpactClass::Other, "\"other\""),
        ] {
            let json = serde_json::to_string(&class).unwrap();
            assert_eq!(json, name);
            let back: ImpactClass = serde_json::from_str(name).unwrap();
            assert_eq!(class, back);
        }
    }

    #[test]
    fn impact_class_rejects_unknown_variants() {
        assert!(serde_json::from_str::<ImpactClass>("\"availability\"").is_err());
        assert!(serde_json::from_str::<ImpactClass>("\"DataLoss\"").is_err());
    }

    #[test]
    fn maximum_severity_policy_follows_plan_section_14() {
        for (class, max) in [
            (ImpactClass::Style, Severity::Low),
            (ImpactClass::Maintainability, Severity::Medium),
            (ImpactClass::TestGap, Severity::Medium),
            (ImpactClass::Performance, Severity::High),
            (ImpactClass::Compatibility, Severity::High),
            (ImpactClass::Other, Severity::High),
            (ImpactClass::Correctness, Severity::Blocker),
            (ImpactClass::Security, Severity::Blocker),
            (ImpactClass::DataLoss, Severity::Blocker),
            (ImpactClass::Deadlock, Severity::Blocker),
            (ImpactClass::Crash, Severity::Blocker),
        ] {
            assert_eq!(
                class.max_severity(),
                max,
                "maximum severity policy mismatch for {class:?}"
            );
        }
    }

    #[test]
    fn review_result_roundtrip() {
        let result = ReviewResult {
            review_id: crate::domain::ids::new_ulid(),
            kind: ReviewKind::Code,
            created_at: crate::domain::time::now_utc_rfc3339(),
            snapshot: sample_snapshot(),
            summary: "Two findings across two reviewers.".to_string(),
            findings: vec![sample_finding()],
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: ReviewResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, back);
        // Spot-check serialized field names.
        assert!(json.contains("\"review_id\""));
        assert!(json.contains("\"cross_validated\""));
        assert!(json.contains("\"source_finding_ids\""));
        assert!(json.contains("\"kind\":\"source\""));
    }
}
