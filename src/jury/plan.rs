//! Deterministic jury plan construction (plan sections 17 and 33).
//!
//! [`build_jury_plan`] applies the frozen jury rules to a
//! [`RoutingDecision`]:
//!
//! 1. `quality` is added deterministically and always holds the first
//!    seat;
//! 2. the selected specialists (at most [`MAX_SELECTED_SPECIALISTS`])
//!    join in routing order, best score first;
//! 3. when no specialist reached the threshold (the dynamic fallback),
//!    the dynamic domain expert joins instead;
//! 4. the resulting jury always has 2..=4 members.
//!
//! The synthesizer is not a juror and never appears in a jury plan.

use std::collections::BTreeSet;

use thiserror::Error;

use crate::domain::jury::{JuryPlan, JuryRole, JurySeat};
use crate::domain::routing::RoutingDecision;
use crate::jury::catalog::{Catalog, DYNAMIC_DOMAIN_EXPERT_ID, QUALITY_ID};

/// Maximum number of scored specialists in one jury (plan section 17).
pub const MAX_SELECTED_SPECIALISTS: usize = 3;

/// Minimum jury size (plan section 17).
pub const MIN_JURY_SIZE: usize = 2;

/// Maximum jury size (plan section 17).
pub const MAX_JURY_SIZE: usize = 4;

/// Jury plan construction failures.
#[derive(Debug, Error, PartialEq)]
pub enum JuryPlanError {
    /// A selected or structural reviewer is absent from the catalog.
    #[error("reviewer `{0}` is not in the catalog")]
    UnknownReviewer(String),
    /// A selected reviewer has no routing score.
    #[error("selected reviewer `{0}` has no routing score")]
    MissingScore(String),
    /// The decision selected more specialists than a jury may hold.
    #[error("decision selects {actual} specialists; at most {max} are allowed")]
    TooManySpecialists {
        /// Number of selected specialists.
        actual: usize,
        /// Frozen maximum.
        max: usize,
    },
    /// The decision combines the fallback with selected specialists.
    #[error("decision marks the dynamic fallback while {actual} specialists are selected")]
    InconsistentFallback {
        /// Number of selected specialists.
        actual: usize,
    },
    /// The jury would violate the frozen size bounds.
    #[error("jury would contain {actual} reviewers; jury size must be 2..=4")]
    InvalidSize {
        /// Resulting jury size.
        actual: usize,
    },
    /// Two seats would name the same reviewer.
    #[error("jury contains duplicate reviewer `{0}`")]
    DuplicateReviewer(String),
}

/// Build the bounded jury plan for a routing decision.
pub fn build_jury_plan(
    decision: &RoutingDecision,
    catalog: &Catalog,
) -> Result<JuryPlan, JuryPlanError> {
    if decision.selected_specialists.len() > MAX_SELECTED_SPECIALISTS {
        return Err(JuryPlanError::TooManySpecialists {
            actual: decision.selected_specialists.len(),
            max: MAX_SELECTED_SPECIALISTS,
        });
    }
    if decision.dynamic_fallback && !decision.selected_specialists.is_empty() {
        return Err(JuryPlanError::InconsistentFallback {
            actual: decision.selected_specialists.len(),
        });
    }

    let mut seats = Vec::with_capacity(MAX_JURY_SIZE);
    let quality = catalog
        .profile(QUALITY_ID)
        .ok_or_else(|| JuryPlanError::UnknownReviewer(QUALITY_ID.to_string()))?;
    seats.push(JurySeat {
        reviewer_id: quality.id.clone(),
        score: None,
        role: JuryRole::Quality,
    });

    for reviewer_id in &decision.selected_specialists {
        let profile = catalog
            .profile(reviewer_id)
            .ok_or_else(|| JuryPlanError::UnknownReviewer(reviewer_id.clone()))?;
        let score = decision
            .scores
            .iter()
            .find(|score| &score.reviewer_id == reviewer_id)
            .map(|score| score.score)
            .ok_or_else(|| JuryPlanError::MissingScore(reviewer_id.clone()))?;
        seats.push(JurySeat {
            reviewer_id: profile.id.clone(),
            score: Some(score),
            role: JuryRole::Specialist,
        });
    }

    if decision.dynamic_fallback {
        let expert = catalog
            .profile(DYNAMIC_DOMAIN_EXPERT_ID)
            .ok_or_else(|| JuryPlanError::UnknownReviewer(DYNAMIC_DOMAIN_EXPERT_ID.to_string()))?;
        seats.push(JurySeat {
            reviewer_id: expert.id.clone(),
            score: None,
            role: JuryRole::DynamicFallback,
        });
    }

    validate_seats(&seats)?;
    Ok(JuryPlan { seats })
}

/// Enforce the frozen jury invariants on the constructed seats.
///
/// The builder guarantees structurally that `quality` holds the
/// first seat (it is pushed unconditionally before any specialist);
/// this check enforces the size bounds and reviewer uniqueness.
fn validate_seats(seats: &[JurySeat]) -> Result<(), JuryPlanError> {
    if !(MIN_JURY_SIZE..=MAX_JURY_SIZE).contains(&seats.len()) {
        return Err(JuryPlanError::InvalidSize {
            actual: seats.len(),
        });
    }
    let mut seen = BTreeSet::new();
    for seat in seats {
        if !seen.insert(seat.reviewer_id.as_str()) {
            return Err(JuryPlanError::DuplicateReviewer(seat.reviewer_id.clone()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::routing::{RoutingInput, RoutingScore};
    use crate::routing::rules::RulesClassifier;

    fn catalog() -> Catalog {
        Catalog::bundled().expect("bundled catalog must load")
    }

    fn decision_for(input: &RoutingInput) -> RoutingDecision {
        RulesClassifier::new(catalog()).classify(input)
    }

    fn score(reviewer_id: &str, value: f64) -> RoutingScore {
        RoutingScore {
            reviewer_id: reviewer_id.to_string(),
            score: value,
            matched_trigger_terms: Vec::new(),
            matched_paths: Vec::new(),
            matched_languages: Vec::new(),
            matched_tags: Vec::new(),
            lesson_boost: 0.0,
        }
    }

    fn decision(scores: Vec<RoutingScore>, selected: &[&str], fallback: bool) -> RoutingDecision {
        RoutingDecision {
            scores,
            selected_specialists: selected.iter().map(|s| s.to_string()).collect(),
            dynamic_fallback: fallback,
            threshold: 0.35,
        }
    }

    #[test]
    fn jury_size_bounds_match_the_plan() {
        assert_eq!(MIN_JURY_SIZE, 2);
        assert_eq!(MAX_JURY_SIZE, 4);
        assert_eq!(MAX_SELECTED_SPECIALISTS, 3);
    }

    #[test]
    fn quality_always_leads_the_jury() {
        // A selected specialist.
        let input = RoutingInput {
            descriptors: vec!["Fix race on the shared queue".to_string()],
            languages: vec!["rust".to_string()],
            deterministic_tags: vec!["concurrency".to_string()],
            ..RoutingInput::default()
        };
        let plan = build_jury_plan(&decision_for(&input), &catalog()).unwrap();
        assert_eq!(plan.reviewer_ids().first(), Some(&"quality"));
        assert_eq!(plan.seats[0].score, None);
        assert_eq!(plan.seats[0].role, JuryRole::Quality);

        // No signal at all: fallback still keeps quality first.
        let plan = build_jury_plan(&decision_for(&RoutingInput::empty()), &catalog()).unwrap();
        assert_eq!(plan.reviewer_ids()[0], "quality");
        assert!(plan.uses_dynamic_fallback());
    }

    #[test]
    fn specialists_keep_routing_order_and_scores() {
        let input = RoutingInput {
            descriptors: vec![
                "Fix race on the shared queue; optimize hot copy path; migrate the users table for auth token validation"
                    .to_string(),
            ],
            changed_paths: vec![
                "db/migrations/0010_users.sql".to_string(),
                "src/auth/login.rs".to_string(),
            ],
            languages: vec!["rust".to_string()],
            ..RoutingInput::default()
        };
        let built = decision_for(&input);
        let plan = build_jury_plan(&built, &catalog()).unwrap();
        let specialists: Vec<&crate::domain::jury::JurySeat> = plan
            .seats
            .iter()
            .filter(|seat| seat.role == JuryRole::Specialist)
            .collect();
        assert_eq!(
            specialists
                .iter()
                .map(|seat| seat.reviewer_id.as_str())
                .collect::<Vec<_>>(),
            built.selected_specialists,
            "specialist seats must mirror routing order"
        );
        for (seat, selected) in specialists.iter().zip(&built.selected_specialists) {
            let expected = built
                .scores
                .iter()
                .find(|score| &score.reviewer_id == selected)
                .unwrap()
                .score;
            assert_eq!(seat.score, Some(expected));
        }
    }

    #[test]
    fn too_many_specialists_is_rejected() {
        let selected = ["security", "concurrency", "database", "performance"];
        let scores = selected.iter().map(|id| score(id, 0.5)).collect();
        let err = build_jury_plan(&decision(scores, &selected, false), &catalog()).unwrap_err();
        assert_eq!(err, JuryPlanError::TooManySpecialists { actual: 4, max: 3 });
    }

    #[test]
    fn fallback_with_selected_specialists_is_rejected() {
        let selected = ["security"];
        let scores = vec![score("security", 0.5)];
        let err = build_jury_plan(&decision(scores, &selected, true), &catalog()).unwrap_err();
        assert_eq!(err, JuryPlanError::InconsistentFallback { actual: 1 });
    }

    #[test]
    fn unknown_reviewer_and_missing_score_are_rejected() {
        let selected = ["not-a-reviewer"];
        let scores = vec![score("not-a-reviewer", 0.5)];
        let err = build_jury_plan(&decision(scores, &selected, false), &catalog()).unwrap_err();
        assert_eq!(
            err,
            JuryPlanError::UnknownReviewer("not-a-reviewer".to_string())
        );

        // Selected reviewer without a score entry.
        let selected = ["security"];
        let err = build_jury_plan(&decision(Vec::new(), &selected, false), &catalog()).unwrap_err();
        assert_eq!(err, JuryPlanError::MissingScore("security".to_string()));
    }

    #[test]
    fn a_single_quality_seat_is_an_invalid_jury() {
        // A decision with no selection and no fallback would produce a
        // one-reviewer jury; construction must refuse it.
        let err = build_jury_plan(&decision(Vec::new(), &[], false), &catalog()).unwrap_err();
        assert_eq!(err, JuryPlanError::InvalidSize { actual: 1 });
    }

    #[test]
    fn duplicate_reviewers_are_rejected() {
        let selected = ["security", "security"];
        let scores = vec![score("security", 0.5)];
        let err = build_jury_plan(&decision(scores, &selected, false), &catalog()).unwrap_err();
        assert_eq!(
            err,
            JuryPlanError::DuplicateReviewer("security".to_string())
        );
    }
}
