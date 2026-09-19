//! Reviewer profiles and read-only reviewer capabilities.
//!
//! Reviewers receive their methodology explicitly: every profile carries an
//! ordered checklist, base instructions, an explicit model profile, and a
//! set of read-only capabilities. A reviewer is never expected to invent its
//! own review methodology.

use serde::{Deserialize, Serialize};

use super::context::ContextEntry;
use super::model::ThinkingLevel;

/// A read-only investigation capability that may be granted to a reviewer
/// session.
///
/// Reviewers never receive edit, write, shell, Git-mutation, or unrestricted
/// filesystem capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewerCapability {
    /// Bounded ranked project-wide context lookup.
    ProjectLookup,
    /// Definition and metadata lookup for a symbol in the target snapshot.
    SymbolLookup,
    /// Source readback restricted to the target snapshot.
    SourceLookup,
    /// Known bounded relationships of a symbol.
    RelationLookup,
}

impl ReviewerCapability {
    /// All capabilities available in Nisse 1.0.
    pub const ALL: [ReviewerCapability; 4] = [
        ReviewerCapability::ProjectLookup,
        ReviewerCapability::SymbolLookup,
        ReviewerCapability::SourceLookup,
        ReviewerCapability::RelationLookup,
    ];

    /// The capability name as used in assets and protocols.
    pub fn name(self) -> &'static str {
        match self {
            ReviewerCapability::ProjectLookup => "project_lookup",
            ReviewerCapability::SymbolLookup => "symbol_lookup",
            ReviewerCapability::SourceLookup => "source_lookup",
            ReviewerCapability::RelationLookup => "relation_lookup",
        }
    }
}

/// A static reviewer profile from the built-in catalog.
///
/// Profiles are frozen assets bundled with the Nisse binary. Routing scores
/// static specialists using `trigger_terms`, `language_hints`, `path_hints`,
/// and `practice_tags`; jury construction adds `quality` deterministically
/// and uses `dynamic-domain-expert` only as the deterministic fallback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewerProfile {
    /// Stable reviewer identifier (lowercase kebab-case).
    pub id: String,
    /// Human-readable name.
    pub display_name: String,
    /// What this reviewer covers and how it participates in juries.
    pub description: String,
    /// Terms that raise this reviewer's routing score when they appear
    /// in the request descriptors or changed symbols of a routing
    /// input. Lesson wording never influences routing (plan section
    /// 51); lessons boost scores only through their routing hints.
    pub trigger_terms: Vec<String>,
    /// Languages from the supported set that strengthen this reviewer's
    /// score when they appear in the change.
    pub language_hints: Vec<String>,
    /// Path patterns (globs over repository-relative POSIX paths) that
    /// strengthen this reviewer's score when changed paths match.
    pub path_hints: Vec<String>,
    /// Practice pack tags whose relevance strengthens this reviewer's score.
    pub practice_tags: Vec<String>,
    /// Explicit ordered review methodology for this reviewer.
    pub checklist: Vec<String>,
    /// Base reviewer instructions used as the session's instruction text.
    pub base_instructions: String,
    /// Named conceptual model profile (`standard`, `strong`, `fast`,
    /// `strongest`) used by default for this reviewer.
    pub default_model_profile: String,
    /// Default thinking level for this reviewer.
    pub default_thinking_level: ThinkingLevel,
    /// Read-only capabilities granted to this reviewer.
    pub allowed_capabilities: Vec<ReviewerCapability>,
}

/// One juror's execution contract (plan section 18).
///
/// Rust constructs exactly one spec per juror. The spec fully determines
/// the reviewer session: instructions, checklist, model profile, thinking
/// level, initial context, read-only capabilities, and explicit bounds.
/// A reviewer can never modify its own execution spec; later milestones
/// map this type onto isolated Pi sessions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewerExecutionSpec {
    /// Stable reviewer identifier of the juror (e.g. `quality`).
    pub reviewer_id: String,
    /// Short description of the reviewer's specialization.
    pub specialization: String,
    /// Instruction text for the session (base instructions plus any
    /// review-specific additions).
    pub instructions: String,
    /// Explicit ordered review methodology for this session.
    pub checklist: Vec<String>,
    /// Named model profile (`standard`, `strong`, …) resolved from trusted
    /// local configuration. Reviewer sessions never inherit a model.
    pub model_profile: String,
    /// Explicit thinking level for the session.
    pub thinking_level: ThinkingLevel,
    /// Strong initial context within budget; reviewer tools only provide
    /// supplemental investigation (plan section 23).
    pub initial_context: Vec<ContextEntry>,
    /// Read-only capabilities granted to this reviewer.
    pub allowed_capabilities: Vec<ReviewerCapability>,
    /// Maximum number of lookup tool calls the reviewer may perform.
    pub max_lookup_calls: u32,
    /// Maximum total bytes of tool-returned context.
    pub max_returned_context_bytes: u64,
    /// Session timeout in milliseconds.
    pub timeout_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_execution_spec() -> ReviewerExecutionSpec {
        ReviewerExecutionSpec {
            reviewer_id: "quality".to_string(),
            specialization: "General correctness".to_string(),
            instructions: "Work the checklist.".to_string(),
            checklist: vec!["Check error paths.".to_string()],
            model_profile: "standard".to_string(),
            thinking_level: ThinkingLevel::Medium,
            initial_context: vec![ContextEntry {
                kind: crate::domain::context::ContextKind::SymbolDefinition,
                title: "function enqueue".to_string(),
                path: Some("src/queue.rs".to_string()),
                symbol: Some("enqueue".to_string()),
                line_start: Some(42),
                line_end: Some(58),
                record_id: None,
                practice_id: None,
                text: "fn enqueue(&mut self, item: T)".to_string(),
                score: 1.0,
            }],
            allowed_capabilities: vec![ReviewerCapability::ProjectLookup],
            max_lookup_calls: 8,
            max_returned_context_bytes: 65_536,
            timeout_ms: 300_000,
        }
    }

    fn sample_profile() -> ReviewerProfile {
        ReviewerProfile {
            id: "quality".to_string(),
            display_name: "Quality".to_string(),
            description: "General correctness reviewer.".to_string(),
            trigger_terms: Vec::new(),
            language_hints: Vec::new(),
            path_hints: Vec::new(),
            practice_tags: vec!["quality".to_string()],
            checklist: vec!["Check error paths.".to_string()],
            base_instructions: "Work the checklist.".to_string(),
            default_model_profile: "standard".to_string(),
            default_thinking_level: ThinkingLevel::Medium,
            allowed_capabilities: ReviewerCapability::ALL.to_vec(),
        }
    }

    #[test]
    fn capability_roundtrip_and_names() {
        for capability in ReviewerCapability::ALL {
            let json = serde_json::to_string(&capability).unwrap();
            let name = capability.name();
            assert_eq!(json, format!("\"{name}\""));
            let back: ReviewerCapability = serde_json::from_str(&json).unwrap();
            assert_eq!(capability, back);
        }
        assert_eq!(
            serde_json::to_string(&ReviewerCapability::ProjectLookup).unwrap(),
            "\"project_lookup\""
        );
    }

    #[test]
    fn capability_rejects_unknown_and_non_readonly_names() {
        assert!(serde_json::from_str::<ReviewerCapability>("\"shell\"").is_err());
        assert!(serde_json::from_str::<ReviewerCapability>("\"edit\"").is_err());
        assert!(serde_json::from_str::<ReviewerCapability>("\"project-lookup\"").is_err());
        assert!(serde_json::from_str::<ReviewerCapability>("\"ProjectLookup\"").is_err());
    }

    #[test]
    fn reviewer_execution_spec_roundtrip() {
        let spec = sample_execution_spec();
        let json = serde_json::to_string(&spec).unwrap();
        let back: ReviewerExecutionSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec, back);
        // Spot-check the frozen wire field names (plan section 18).
        for field in [
            "\"reviewer_id\"",
            "\"specialization\"",
            "\"instructions\"",
            "\"checklist\"",
            "\"model_profile\"",
            "\"thinking_level\"",
            "\"initial_context\"",
            "\"allowed_capabilities\"",
            "\"max_lookup_calls\"",
            "\"max_returned_context_bytes\"",
            "\"timeout_ms\"",
        ] {
            assert!(json.contains(field), "missing field {field} in {json}");
        }
    }

    #[test]
    fn reviewer_execution_spec_is_strict() {
        let spec = sample_execution_spec();
        let json = serde_json::to_string(&spec).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();

        // Unknown fields are rejected.
        let mut extra = value.clone();
        extra
            .as_object_mut()
            .unwrap()
            .insert("tools".to_string(), serde_json::json!(["shell"]));
        assert!(
            serde_json::from_str::<ReviewerExecutionSpec>(&serde_json::to_string(&extra).unwrap())
                .is_err()
        );

        // Every declared field is required.
        for field in [
            "reviewer_id",
            "specialization",
            "instructions",
            "checklist",
            "model_profile",
            "thinking_level",
            "initial_context",
            "allowed_capabilities",
            "max_lookup_calls",
            "max_returned_context_bytes",
            "timeout_ms",
        ] {
            let mut pruned = value.clone();
            assert!(pruned.as_object_mut().unwrap().remove(field).is_some());
            let pruned_json = serde_json::to_string(&pruned).unwrap();
            assert!(
                serde_json::from_str::<ReviewerExecutionSpec>(&pruned_json).is_err(),
                "expected rejection when `{field}` is missing"
            );
        }
    }

    #[test]
    fn reviewer_profile_roundtrip() {
        let profile = sample_profile();
        let json = serde_json::to_string(&profile).unwrap();
        let back: ReviewerProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(profile, back);
        assert!(json.contains("\"default_model_profile\":\"standard\""));
        assert!(json.contains("\"default_thinking_level\":\"medium\""));
        assert!(json.contains("\"allowed_capabilities\""));
    }

    #[test]
    fn reviewer_profile_is_strict() {
        let profile = sample_profile();
        let json = serde_json::to_string(&profile).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();

        // Unknown fields are rejected.
        let mut extra = value.clone();
        extra
            .as_object_mut()
            .unwrap()
            .insert("temperature".to_string(), serde_json::json!(0.2));
        assert!(
            serde_json::from_str::<ReviewerProfile>(&serde_json::to_string(&extra).unwrap())
                .is_err()
        );

        // Every declared field is required.
        for field in [
            "id",
            "display_name",
            "description",
            "trigger_terms",
            "language_hints",
            "path_hints",
            "practice_tags",
            "checklist",
            "base_instructions",
            "default_model_profile",
            "default_thinking_level",
            "allowed_capabilities",
        ] {
            let mut pruned = value.clone();
            assert!(pruned.as_object_mut().unwrap().remove(field).is_some());
            let pruned_json = serde_json::to_string(&pruned).unwrap();
            assert!(
                serde_json::from_str::<ReviewerProfile>(&pruned_json).is_err(),
                "expected rejection when `{field}` is missing"
            );
        }
    }
}
