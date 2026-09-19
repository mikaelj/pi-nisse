//! Jury plans (plan sections 13 and 17).
//!
//! A [`JuryPlan`] is the frozen, bounded composition of a review jury:
//! `quality` always participates, joined by one to three scored
//! specialists, or by the dynamic domain expert when no specialist
//! reaches the routing threshold. The synthesizer is not a juror; it
//! never appears in a jury plan.

use serde::{Deserialize, Serialize};

/// Why a reviewer holds a jury seat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JuryRole {
    /// The `quality` reviewer, added deterministically to every jury;
    /// never selected by routing scores.
    Quality,
    /// A static specialist selected by routing score.
    Specialist,
    /// The dynamic domain expert, added only when no static specialist
    /// reaches the routing threshold.
    DynamicFallback,
}

/// One seat in a jury plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JurySeat {
    /// Reviewer id.
    pub reviewer_id: String,
    /// Routing score for specialists; `None` for structurally added
    /// reviewers (`quality`, dynamic fallback).
    pub score: Option<f64>,
    /// Why this reviewer participates.
    pub role: JuryRole,
}

/// The bounded composition of a review jury (plan section 17).
///
/// Seats are ordered deterministically: `quality` first, then selected
/// specialists best score first (frozen catalog order breaks ties),
/// then the dynamic domain expert when the fallback fired. Jury size is
/// always 2..=4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JuryPlan {
    /// Jury seats in deterministic order.
    pub seats: Vec<JurySeat>,
}

impl JuryPlan {
    /// Reviewer ids in seat order.
    pub fn reviewer_ids(&self) -> Vec<&str> {
        self.seats
            .iter()
            .map(|seat| seat.reviewer_id.as_str())
            .collect()
    }

    /// Whether the reviewer with this id holds a seat.
    pub fn contains(&self, reviewer_id: &str) -> bool {
        self.seats
            .iter()
            .any(|seat| seat.reviewer_id == reviewer_id)
    }

    /// Number of seats.
    pub fn size(&self) -> usize {
        self.seats.len()
    }

    /// Whether the dynamic domain expert fallback fired for this jury.
    pub fn uses_dynamic_fallback(&self) -> bool {
        self.seats
            .iter()
            .any(|seat| seat.role == JuryRole::DynamicFallback)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seat(reviewer_id: &str, score: Option<f64>, role: JuryRole) -> JurySeat {
        JurySeat {
            reviewer_id: reviewer_id.to_string(),
            score,
            role,
        }
    }

    fn sample_plan() -> JuryPlan {
        JuryPlan {
            seats: vec![
                seat("quality", None, JuryRole::Quality),
                seat("concurrency", Some(0.6), JuryRole::Specialist),
            ],
        }
    }

    #[test]
    fn jury_role_roundtrip() {
        for (role, name) in [
            (JuryRole::Quality, "\"quality\""),
            (JuryRole::Specialist, "\"specialist\""),
            (JuryRole::DynamicFallback, "\"dynamic_fallback\""),
        ] {
            let json = serde_json::to_string(&role).unwrap();
            assert_eq!(json, name);
            let back: JuryRole = serde_json::from_str(name).unwrap();
            assert_eq!(role, back);
        }
        assert!(serde_json::from_str::<JuryRole>("\"synthesizer\"").is_err());
    }

    #[test]
    fn jury_plan_roundtrip_and_accessors() {
        let plan = sample_plan();
        let json = serde_json::to_string(&plan).unwrap();
        let back: JuryPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(plan, back);
        assert!(json.contains("\"score\":0.6"));
        assert!(json.contains("\"score\":null"));

        assert_eq!(plan.reviewer_ids(), vec!["quality", "concurrency"]);
        assert!(plan.contains("quality"));
        assert!(!plan.contains("security"));
        assert_eq!(plan.size(), 2);
        assert!(!plan.uses_dynamic_fallback());
    }

    #[test]
    fn uses_dynamic_fallback_detects_expert_seat() {
        let plan = JuryPlan {
            seats: vec![
                seat("quality", None, JuryRole::Quality),
                seat("dynamic-domain-expert", None, JuryRole::DynamicFallback),
            ],
        };
        assert!(plan.uses_dynamic_fallback());
    }

    #[test]
    fn jury_plan_is_strict() {
        let json = serde_json::to_string(&sample_plan()).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("synthesizer".to_string(), serde_json::json!("gpt"));
        assert!(serde_json::from_str::<JuryPlan>(&serde_json::to_string(&value).unwrap()).is_err());

        let mut pruned = value;
        assert!(pruned.as_object_mut().unwrap().remove("seats").is_some());
        assert!(
            serde_json::from_str::<JuryPlan>(&serde_json::to_string(&pruned).unwrap()).is_err()
        );
    }
}
