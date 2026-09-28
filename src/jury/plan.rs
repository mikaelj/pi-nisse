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
use crate::jury::catalog::{
    CLAIM_SKEPTIC_ID, COVERAGE_AUDITOR_ID, Catalog, DYNAMIC_DOMAIN_EXPERT_ID, QUALITY_ID,
};

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

/// Build the frozen INVESTIGATION jury plan (PLAN_INVESTIGATION.md and
/// the section 17 amendment):
///
///     quality + claim-skeptic + coverage-auditor + one domain reviewer
///
/// The domain seat takes the best-scoring specialist from the routing
/// decision (exactly one), or the dynamic fallback when no specialist
/// reaches the threshold. Claim review and coverage review are separate
/// review tasks in separate isolated sessions (invariant 30); they are
/// structural, never routed by score.
pub fn build_investigation_jury_plan(
    decision: &RoutingDecision,
    catalog: &Catalog,
) -> Result<JuryPlan, JuryPlanError> {
    let structural = |id: &str, role: JuryRole| -> Result<JurySeat, JuryPlanError> {
        let profile = catalog
            .profile(id)
            .ok_or_else(|| JuryPlanError::UnknownReviewer(id.to_string()))?;
        Ok(JurySeat {
            reviewer_id: profile.id.clone(),
            score: None,
            role,
        })
    };

    let mut seats = Vec::with_capacity(MAX_JURY_SIZE);
    seats.push(structural(QUALITY_ID, JuryRole::Quality)?);
    seats.push(structural(CLAIM_SKEPTIC_ID, JuryRole::InvestigationClaim)?);
    seats.push(structural(
        COVERAGE_AUDITOR_ID,
        JuryRole::InvestigationCoverage,
    )?);

    // Exactly one domain reviewer: the best-scoring specialist, or the
    // dynamic fallback when none qualifies.
    let domain: Option<(String, Option<f64>, bool)> = decision
        .selected_specialists
        .first()
        .map(|reviewer_id| {
            let score = decision
                .scores
                .iter()
                .find(|score| &score.reviewer_id == reviewer_id)
                .map(|score| score.score);
            (reviewer_id.clone(), score, false)
        })
        .or_else(|| {
            decision
                .dynamic_fallback
                .then(|| (DYNAMIC_DOMAIN_EXPERT_ID.to_string(), None, true))
        });
    match domain {
        Some((reviewer_id, score, fallback)) => seats.push(JurySeat {
            reviewer_id,
            score,
            role: if fallback {
                JuryRole::DynamicFallback
            } else {
                JuryRole::Specialist
            },
        }),
        // No specialist and no fallback flag cannot happen for a
        // well-formed decision, but fail closed rather than guess.
        None => {
            return Err(JuryPlanError::InconsistentFallback {
                actual: decision.selected_specialists.len(),
            });
        }
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

    #[test]
    fn investigation_jury_is_quality_claim_coverage_plus_one_domain() {
        // A routing decision that selects specialists: only the best one
        // joins the investigation jury.
        let decision = RoutingDecision {
            scores: vec![
                RoutingScore::for_test("security", 0.9),
                RoutingScore::for_test("networking", 0.8),
            ],
            selected_specialists: vec!["security".to_string(), "networking".to_string()],
            dynamic_fallback: false,
            threshold: 0.35,
        };
        let plan = build_investigation_jury_plan(&decision, &catalog()).unwrap();
        let ids: Vec<&str> = plan.seats.iter().map(|s| s.reviewer_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["quality", "claim-skeptic", "coverage-auditor", "security"]
        );
        assert_eq!(plan.seats[1].role, JuryRole::InvestigationClaim);
        assert_eq!(plan.seats[2].role, JuryRole::InvestigationCoverage);
        assert_eq!(plan.seats[3].role, JuryRole::Specialist);
        assert_eq!(plan.seats[3].score, Some(0.9));
    }

    #[test]
    fn investigation_jury_uses_dynamic_fallback_when_no_specialist_scores() {
        let decision = RoutingDecision {
            scores: vec![],
            selected_specialists: vec![],
            dynamic_fallback: true,
            threshold: 0.35,
        };
        let plan = build_investigation_jury_plan(&decision, &catalog()).unwrap();
        let ids: Vec<&str> = plan.seats.iter().map(|s| s.reviewer_id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "quality",
                "claim-skeptic",
                "coverage-auditor",
                "dynamic-domain-expert"
            ]
        );
        assert_eq!(plan.seats[3].role, JuryRole::DynamicFallback);
    }

    #[test]
    fn routing_never_scores_structural_investigation_reviewers() {
        let classifier = RulesClassifier::new(catalog());
        let input = RoutingInput {
            descriptors: vec!["claim-skeptic coverage-auditor investigation".to_string()],
            ..RoutingInput::empty()
        };
        let decision = classifier.classify(&input);
        let scored: Vec<&str> = decision
            .scores
            .iter()
            .map(|s| s.reviewer_id.as_str())
            .collect();
        assert!(!scored.contains(&"claim-skeptic"));
        assert!(!scored.contains(&"coverage-auditor"));
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
