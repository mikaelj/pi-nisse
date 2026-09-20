//! Deterministic integration of optional routing providers (plan
//! sections 33–34).
//!
//! An optional provider (the Needle classifier) may propose scores for
//! the static specialists. Nisse never trusts those proposals
//! directly:
//!
//! 1. every proposal is validated — reviewer ids must name real
//!    static specialists of the catalog (never `quality`, never
//!    `dynamic-domain-expert`, which are structural seats), scores
//!    must be finite within `[0.0, 1.0]`, and ids must be unique. Any
//!    violation rejects the *whole* response: invalid output causes
//!    less automation, not guessed state;
//! 2. accepted scores are blended into the mandatory rules decision
//!    with fixed weights and clamped to `[0.0, 1.0]`; specialists the
//!    provider did not score keep their rules score;
//! 3. selection reuses the exact rules selection
//!    ([`finalize_decision`](crate::routing::rules)), so the frozen
//!    jury constraints cannot be bypassed: `quality` always
//!    participates, at most three specialists join, the jury totals
//!    at most four, and the dynamic fallback fires only when no
//!    specialist reaches the threshold.
//!
//! A provider that is unconfigured, unreachable, or rejected leaves
//! the rules decision untouched — the fallback is the mandatory
//! deterministic behavior.

use std::collections::{BTreeMap, BTreeSet};

use crate::domain::routing::{RoutingDecision, RoutingInput, RoutingScore};
use crate::jury::catalog::{Catalog, DYNAMIC_DOMAIN_EXPERT_ID, QUALITY_ID};
use crate::routing::rules::finalize_decision;

use super::{ProviderScore, RoutingClassifierProvider};

/// Weight of the provider score in the blend (the rules score keeps
/// `1.0 - PROVIDER_SCORE_WEIGHT`).
pub const PROVIDER_SCORE_WEIGHT: f64 = 0.5;

/// Maximum byte length of a provider reviewer id (mirrors the adapter
/// bound; validated again here as defense in depth).
const MAX_REVIEWER_ID_BYTES: usize = 64;

/// Blend an optional provider into the rules routing decision.
///
/// When no provider is configured the rules decision is returned
/// unchanged. When the provider fails (timeout, crash, untrusted or
/// malformed output) the rules decision is returned unchanged plus a
/// diagnostic, so reviews never depend on provider availability.
pub fn classify_with_optional_provider(
    rules: RoutingDecision,
    provider: Option<&dyn RoutingClassifierProvider>,
    input: &RoutingInput,
    catalog: &Catalog,
) -> (RoutingDecision, Vec<String>) {
    let Some(provider) = provider else {
        return (rules, Vec::new());
    };
    match provider.route(input) {
        Ok(scores) => apply_provider_scores(rules, provider.name(), &scores, catalog),
        Err(err) => (
            rules,
            vec![format!(
                "{} routing unavailable ({err}); using rules-only routing",
                provider.name()
            )],
        ),
    }
}

/// Validate provider scores and blend them into `decision`.
///
/// Any invalid entry rejects the entire response conservatively; the
/// caller keeps the rules decision and receives a diagnostic.
pub fn apply_provider_scores(
    decision: RoutingDecision,
    provider: &str,
    scores: &[ProviderScore],
    catalog: &Catalog,
) -> (RoutingDecision, Vec<String>) {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for score in scores {
        if score.reviewer_id.is_empty() || score.reviewer_id.len() > MAX_REVIEWER_ID_BYTES {
            return rejected(
                decision,
                provider,
                "a reviewer id is empty or oversized".to_string(),
            );
        }
        if !seen.insert(&score.reviewer_id) {
            return rejected(
                decision,
                provider,
                format!("reviewer `{}` scored twice", score.reviewer_id),
            );
        }
        if !score.score.is_finite() || !(0.0..=1.0).contains(&score.score) {
            return rejected(
                decision,
                provider,
                format!("score for `{}` is outside [0.0, 1.0]", score.reviewer_id),
            );
        }
        // Reviewer-ID validation: only real static specialists of the
        // frozen catalog may be scored; the structural seats are
        // deterministic and never provider-selectable.
        let Some(profile) = catalog.profile(&score.reviewer_id) else {
            return rejected(
                decision,
                provider,
                format!(
                    "unknown reviewer `{}` is not in the static catalog",
                    score.reviewer_id
                ),
            );
        };
        if profile.id == QUALITY_ID || profile.id == DYNAMIC_DOMAIN_EXPERT_ID {
            return rejected(
                decision,
                provider,
                format!(
                    "structural reviewer `{}` may not be provider-scored",
                    profile.id
                ),
            );
        }
    }

    // An empty valid response is a no-op (the provider abstains).
    if scores.is_empty() {
        return (decision, Vec::new());
    }

    let proposals: BTreeMap<&str, f64> = scores
        .iter()
        .map(|score| (score.reviewer_id.as_str(), score.score))
        .collect();
    let rules_weight = 1.0 - PROVIDER_SCORE_WEIGHT;
    let threshold = decision.threshold;
    let blended: Vec<RoutingScore> = decision
        .scores
        .into_iter()
        .map(|mut score| {
            if let Some(proposal) = proposals.get(score.reviewer_id.as_str()) {
                score.score =
                    (rules_weight * score.score + PROVIDER_SCORE_WEIGHT * proposal).min(1.0);
            }
            score
        })
        .collect();

    // Selection reuses the frozen rules selection: threshold, at most
    // three specialists, dynamic fallback. Jury construction then
    // enforces the bounds again deterministically.
    (finalize_decision(blended, threshold), Vec::new())
}

/// Return the untouched rules decision plus one rejection diagnostic.
fn rejected(
    decision: RoutingDecision,
    provider: &str,
    message: String,
) -> (RoutingDecision, Vec<String>) {
    (
        decision,
        vec![format!(
            "{provider} routing output rejected ({message}); using rules-only routing"
        )],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::routing::RoutingInput;
    use crate::routing::rules::RulesClassifier;
    use thiserror::Error;

    fn catalog() -> Catalog {
        Catalog::bundled().expect("bundled catalog must load")
    }

    fn decision_for(input: &RoutingInput) -> RoutingDecision {
        RulesClassifier::new(catalog()).classify(input)
    }

    fn score(id: &str, value: f64) -> ProviderScore {
        ProviderScore {
            reviewer_id: id.to_string(),
            score: value,
        }
    }

    fn queue_input() -> RoutingInput {
        RoutingInput {
            descriptors: vec!["Fix the race on the shared queue".to_string()],
            changed_paths: vec!["src/queue.rs".to_string()],
            languages: vec!["rust".to_string()],
            changed_symbols: Vec::new(),
            deterministic_tags: vec!["concurrency".to_string()],
            lessons: Vec::new(),
        }
    }

    #[derive(Debug, Error)]
    #[error("scripted provider failure")]
    struct ScriptedFailure;

    /// A scripted provider for the fallback paths.
    struct ScriptedProvider {
        name: &'static str,
        result: Result<Vec<ProviderScore>, ScriptedFailure>,
    }

    impl RoutingClassifierProvider for ScriptedProvider {
        fn name(&self) -> &'static str {
            self.name
        }

        fn route(
            &self,
            _input: &RoutingInput,
        ) -> Result<Vec<ProviderScore>, super::super::ProviderError> {
            match &self.result {
                Ok(scores) => Ok(scores.clone()),
                Err(_) => Err(super::super::ProviderError::Timeout {
                    provider: self.name.to_string(),
                    after_ms: 5,
                }),
            }
        }
    }

    #[test]
    fn valid_scores_blend_into_the_rules_decision() {
        let input = queue_input();
        let rules = decision_for(&input);
        assert_eq!(rules.selected_specialists, vec!["concurrency"]);

        // `testing` has no rules signal here; the provider proposal
        // lifts it over the threshold without touching concurrency.
        let (decision, diagnostics) = apply_provider_scores(
            rules.clone(),
            "needle",
            &[score("testing", 0.9), score("concurrency", 1.0)],
            &catalog(),
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(
            decision.selected_specialists,
            vec!["concurrency", "testing"],
            "best first"
        );

        // Blend math: concurrency rules score ~0.9 (trigger+path+lang+tag)
        // blended with 1.0 stays at or above 0.9 and at most 1.0.
        let concurrency = decision
            .scores
            .iter()
            .find(|s| s.reviewer_id == "concurrency")
            .unwrap();
        let rules_score = rules
            .scores
            .iter()
            .find(|s| s.reviewer_id == "concurrency")
            .unwrap()
            .score;
        let expected =
            (rules_score * (1.0 - PROVIDER_SCORE_WEIGHT) + PROVIDER_SCORE_WEIGHT * 1.0).min(1.0);
        assert!((concurrency.score - expected).abs() < 1e-12);

        // Reviewers the provider did not score keep their rules score.
        let untouched = decision
            .scores
            .iter()
            .find(|s| s.reviewer_id == "security")
            .unwrap();
        let rules_untouched = rules
            .scores
            .iter()
            .find(|s| s.reviewer_id == "security")
            .unwrap();
        assert_eq!(untouched.score, rules_untouched.score);
    }

    #[test]
    fn provider_scores_cannot_bypass_the_jury_constraints() {
        let input = queue_input();
        let rules = decision_for(&input);

        // Five strong specialist proposals: still at most three
        // specialists, `quality` leads the jury, total at most four.
        let proposals = vec![
            score("security", 1.0),
            score("testing", 1.0),
            score("database", 1.0),
            score("performance", 1.0),
            score("maintainability", 1.0),
        ];
        let (decision, _) = apply_provider_scores(rules, "needle", &proposals, &catalog());
        assert_eq!(decision.selected_specialists.len(), 3);
        let plan = crate::jury::plan::build_jury_plan(&decision, &catalog()).unwrap();
        assert_eq!(plan.size(), 4);
        assert_eq!(plan.reviewer_ids().first().copied(), Some("quality"));

        // A provider cannot force the structural seats.
        let (decision, diagnostics) = apply_provider_scores(
            decision_for(&input),
            "needle",
            &[score("quality", 1.0)],
            &catalog(),
        );
        assert!(!diagnostics.is_empty());
        assert_eq!(decision.selected_specialists, vec!["concurrency"]);
    }

    #[test]
    fn provider_may_lower_but_never_break_a_jury() {
        let input = queue_input();
        let rules = decision_for(&input);
        assert!(!rules.dynamic_fallback);

        // Lowering every signal below the threshold fires the
        // deterministic dynamic fallback: the jury stays valid.
        let proposals: Vec<ProviderScore> = rules
            .scores
            .iter()
            .map(|s| score(&s.reviewer_id, 0.0))
            .collect();
        let (decision, diagnostics) =
            apply_provider_scores(rules, "needle", &proposals, &catalog());
        assert!(diagnostics.is_empty());
        assert!(decision.dynamic_fallback);
        let plan = crate::jury::plan::build_jury_plan(&decision, &catalog()).unwrap();
        assert_eq!(
            plan.reviewer_ids(),
            vec!["quality", "dynamic-domain-expert"]
        );
    }

    #[test]
    fn unknown_reviewers_reject_the_whole_response() {
        let input = queue_input();
        let rules = decision_for(&input);

        for invalid in [
            vec![score("made-up-reviewer", 0.9)],
            vec![score("dynamic-domain-expert", 0.9)],
            vec![score("quality", 0.9)],
            vec![score("", 0.9)],
            vec![score(&"x".repeat(65), 0.9)],
            vec![score("concurrency", 1.5)],
            vec![score("concurrency", f64::NAN)],
            vec![score("concurrency", 0.5), score("concurrency", 0.6)],
        ] {
            let (decision, diagnostics) =
                apply_provider_scores(rules.clone(), "needle", &invalid, &catalog());
            assert_eq!(
                decision, rules,
                "the rules decision must survive rejection of {invalid:?}"
            );
            assert_eq!(diagnostics.len(), 1, "{invalid:?}");
            assert!(
                diagnostics[0].contains("needle routing output rejected"),
                "{}",
                diagnostics[0]
            );
        }
    }

    #[test]
    fn empty_and_absent_providers_are_no_ops() {
        let input = queue_input();
        let rules = decision_for(&input);

        let (decision, diagnostics) =
            apply_provider_scores(rules.clone(), "needle", &[], &catalog());
        assert_eq!(decision, rules);
        assert!(diagnostics.is_empty());

        let (decision, diagnostics) =
            classify_with_optional_provider(rules.clone(), None, &input, &catalog());
        assert_eq!(decision, rules);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn failing_providers_fall_back_to_the_rules_decision() {
        let input = queue_input();
        let rules = decision_for(&input);
        let provider = ScriptedProvider {
            name: "needle",
            result: Err(ScriptedFailure),
        };
        let (decision, diagnostics) =
            classify_with_optional_provider(rules.clone(), Some(&provider), &input, &catalog());
        assert_eq!(decision, rules);
        assert_eq!(diagnostics.len(), 1);
        assert!(
            diagnostics[0].contains("needle routing unavailable"),
            "{}",
            diagnostics[0]
        );
        assert!(
            diagnostics[0].contains("timed out after 5 ms"),
            "{}",
            diagnostics[0]
        );

        // Successful scripted provider flows through the blend.
        let provider = ScriptedProvider {
            name: "needle",
            result: Ok(vec![score("testing", 0.9)]),
        };
        let (decision, diagnostics) =
            classify_with_optional_provider(rules, Some(&provider), &input, &catalog());
        assert!(diagnostics.is_empty());
        assert!(
            decision
                .selected_specialists
                .contains(&"testing".to_string())
        );
    }
}
