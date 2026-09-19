//! Routing input and decision types (plan sections 13 and 33).
//!
//! Routing selects review juries deterministically. A
//! [`RoutingInput`] carries the deterministic descriptors of a review
//! request; the rules classifier turns it into a [`RoutingDecision`]
//! over the static specialist catalog. Jury construction then applies
//! the frozen jury rules: `quality` always participates, at most three
//! scored specialists join, the jury never exceeds four reviewers, and
//! the dynamic domain expert is the deterministic fallback when no
//! specialist reaches the threshold.
//!
//! Lesson wording never influences routing (plan section 51); lessons
//! participate only through their deterministic routing hints.

use serde::{Deserialize, Serialize};

use super::records::RoutingHint;

/// Deterministic input to reviewer routing (plan section 33).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingInput {
    /// Free-text request descriptors: plan title and section headings,
    /// commit message, diff subjects, and similar short texts.
    pub descriptors: Vec<String>,
    /// Repository-relative POSIX paths changed by the request.
    pub changed_paths: Vec<String>,
    /// Lowercase index language ids detected in the change.
    pub languages: Vec<String>,
    /// Symbols changed or directly affected by the request.
    pub changed_symbols: Vec<String>,
    /// Deterministic tags derived from relevant practices, facts, and
    /// lessons.
    pub deterministic_tags: Vec<String>,
    /// Routing hints of relevant lessons. Boosts may only raise scores
    /// (plan section 33); lesson text never controls routing (plan
    /// section 51).
    pub lessons: Vec<RoutingHint>,
}

impl RoutingInput {
    /// The empty routing input (no signals). Routing still produces a
    /// valid jury via the dynamic-domain-expert fallback.
    pub fn empty() -> Self {
        RoutingInput::default()
    }
}

/// One static specialist's routing score with its matched signals.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingScore {
    /// Reviewer id.
    pub reviewer_id: String,
    /// Final score in `[0.0, 1.0]`, including lesson boosts.
    pub score: f64,
    /// Trigger terms of the reviewer matched in descriptors or changed
    /// symbols (sorted, deduplicated).
    pub matched_trigger_terms: Vec<String>,
    /// Changed paths matched by the reviewer's path hints (sorted,
    /// deduplicated).
    pub matched_paths: Vec<String>,
    /// Input languages covered by the reviewer's language hints
    /// (sorted, deduplicated).
    pub matched_languages: Vec<String>,
    /// Input tags covered by the reviewer's practice tags (sorted,
    /// deduplicated).
    pub matched_tags: Vec<String>,
    /// Total lesson boost applied to this score, capped by plan
    /// section 51.
    pub lesson_boost: f64,
}

/// The result of deterministic routing over the static catalog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingDecision {
    /// Scores of every static specialist, best first (frozen catalog
    /// order breaks ties).
    pub scores: Vec<RoutingScore>,
    /// Selected specialists (at most three), best first.
    pub selected_specialists: Vec<String>,
    /// Whether the dynamic domain expert fallback fired (no specialist
    /// reached the threshold).
    pub dynamic_fallback: bool,
    /// The selection threshold used.
    pub threshold: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::records::testutil::{lesson, ulid};

    fn sample_input() -> RoutingInput {
        let mut record = lesson("alice");
        record.id = ulid();
        RoutingInput {
            descriptors: vec!["Fix race in the queue".to_string()],
            changed_paths: vec!["src/queue.rs".to_string()],
            languages: vec!["rust".to_string()],
            changed_symbols: vec!["enqueue".to_string()],
            deterministic_tags: vec!["concurrency".to_string()],
            lessons: vec![record.routing_hint.clone()],
        }
    }

    fn sample_decision() -> RoutingDecision {
        RoutingDecision {
            scores: vec![RoutingScore {
                reviewer_id: "concurrency".to_string(),
                score: 0.6,
                matched_trigger_terms: vec!["queue".to_string(), "race".to_string()],
                matched_paths: Vec::new(),
                matched_languages: vec!["rust".to_string()],
                matched_tags: vec!["concurrency".to_string()],
                lesson_boost: 0.0,
            }],
            selected_specialists: vec!["concurrency".to_string()],
            dynamic_fallback: false,
            threshold: 0.35,
        }
    }

    #[test]
    fn routing_input_roundtrip_and_default() {
        let input = sample_input();
        let json = serde_json::to_string(&input).unwrap();
        let back: RoutingInput = serde_json::from_str(&json).unwrap();
        assert_eq!(input, back);
        assert!(json.contains("\"deterministic_tags\""));
        assert!(json.contains("\"changed_symbols\""));

        let empty = RoutingInput::empty();
        assert_eq!(empty, RoutingInput::default());
        let back: RoutingInput =
            serde_json::from_str(&serde_json::to_string(&empty).unwrap()).unwrap();
        assert_eq!(back, empty);
    }

    #[test]
    fn routing_input_is_strict() {
        let json = serde_json::to_string(&sample_input()).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("needle_embedding".to_string(), serde_json::json!(true));
        assert!(
            serde_json::from_str::<RoutingInput>(&serde_json::to_string(&value).unwrap()).is_err()
        );

        let mut pruned = value.clone();
        assert!(
            pruned
                .as_object_mut()
                .unwrap()
                .remove("descriptors")
                .is_some()
        );
        assert!(
            serde_json::from_str::<RoutingInput>(&serde_json::to_string(&pruned).unwrap()).is_err()
        );
    }

    #[test]
    fn routing_score_and_decision_roundtrip() {
        let decision = sample_decision();
        let json = serde_json::to_string(&decision).unwrap();
        let back: RoutingDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(decision, back);
        assert!(json.contains("\"dynamic_fallback\":false"));
        assert!(json.contains("\"lesson_boost\":0.0"));
        assert!(json.contains("\"matched_trigger_terms\""));
    }

    #[test]
    fn routing_decision_is_strict() {
        let json = serde_json::to_string(&sample_decision()).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("jury".to_string(), serde_json::json!([]));
        assert!(
            serde_json::from_str::<RoutingDecision>(&serde_json::to_string(&value).unwrap())
                .is_err()
        );

        let mut pruned = value;
        assert!(
            pruned
                .as_object_mut()
                .unwrap()
                .remove("threshold")
                .is_some()
        );
        assert!(
            serde_json::from_str::<RoutingDecision>(&serde_json::to_string(&pruned).unwrap())
                .is_err()
        );
    }
}
