//! Reviewer profiles and read-only reviewer capabilities.
//!
//! Reviewers receive their methodology explicitly: every profile carries an
//! ordered checklist, base instructions, an explicit model profile, and a
//! set of read-only capabilities. A reviewer is never expected to invent its
//! own review methodology.

use serde::{Deserialize, Serialize};

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

#[cfg(test)]
mod tests {
    use super::*;

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
