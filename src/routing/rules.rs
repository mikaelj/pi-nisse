//! The mandatory deterministic rules classifier (plan section 33).
//!
//! [`RulesClassifier`] scores every static specialist of the reviewer
//! catalog against a [`RoutingInput`] using only deterministic
//! signals:
//!
//! - trigger terms, matched as normalized word-boundary phrases in the
//!   request descriptors and changed symbols;
//! - path hints, matched as repository-relative globs over changed
//!   paths;
//! - language hints, matched against the change's detected languages;
//! - practice tags, matched against deterministic input tags;
//! - lesson boosts, applied from validated lesson routing hints.
//!
//! Lessons may only increase scores, never suppress a reviewer (plan
//! section 33). Selection takes up to
//! [`MAX_SELECTED_SPECIALISTS`](jury::plan::MAX_SELECTED_SPECIALISTS)
//! specialists at or above the threshold; the dynamic domain expert is
//! never scored and is added only as the deterministic fallback by jury
//! construction.

use std::collections::BTreeSet;

use crate::domain::records::{MAX_ROUTING_BOOST, RoutingHint};
use crate::domain::routing::{RoutingDecision, RoutingInput, RoutingScore};
use crate::jury::catalog::{Catalog, DYNAMIC_DOMAIN_EXPERT_ID, QUALITY_ID};
use crate::jury::plan::MAX_SELECTED_SPECIALISTS;
use crate::routing::pathglob::glob_match;

/// Default specialist selection threshold (plan section 33).
pub const DEFAULT_THRESHOLD: f64 = 0.35;

/// Weight of matched trigger terms in a specialist's base score.
pub const TRIGGER_WEIGHT: f64 = 0.5;
/// Weight of path-hint matches.
pub const PATH_WEIGHT: f64 = 0.2;
/// Weight of language-hint matches.
pub const LANGUAGE_WEIGHT: f64 = 0.2;
/// Weight of practice-tag matches.
pub const TAG_WEIGHT: f64 = 0.1;
/// Number of matched trigger terms at which the trigger signal
/// saturates.
pub const TRIGGER_SATURATION_TERMS: usize = 3;

/// The mandatory rules-based routing classifier. Routing always works
/// without any LLM, Needle, or embeddings (permanent invariant 14).
#[derive(Debug, Clone)]
pub struct RulesClassifier {
    catalog: Catalog,
    threshold: f64,
}

impl RulesClassifier {
    /// Create a classifier over a validated catalog using the default
    /// threshold.
    pub fn new(catalog: Catalog) -> Self {
        RulesClassifier {
            catalog,
            threshold: DEFAULT_THRESHOLD,
        }
    }

    /// Override the selection threshold. A non-finite threshold
    /// conservatively selects no specialist (the dynamic fallback
    /// fires).
    pub fn with_threshold(mut self, threshold: f64) -> Self {
        self.threshold = threshold;
        self
    }

    /// The selection threshold in use.
    pub fn threshold(&self) -> f64 {
        self.threshold
    }

    /// Classify a routing input into a routing decision.
    ///
    /// Deterministic: the same catalog and input always produce the
    /// same decision. Scores are sorted best first with frozen catalog
    /// order breaking ties; `quality` and `dynamic-domain-expert` are
    /// structural and never scored.
    pub fn classify(&self, input: &RoutingInput) -> RoutingDecision {
        let sources: Vec<Vec<String>> = input
            .descriptors
            .iter()
            .chain(&input.changed_symbols)
            .map(|text| normalized_tokens(text))
            .collect();
        let languages: BTreeSet<String> =
            input.languages.iter().map(|l| l.to_lowercase()).collect();
        let tags: BTreeSet<String> = input
            .deterministic_tags
            .iter()
            .map(|tag| tag.to_lowercase())
            .collect();

        let mut scores: Vec<RoutingScore> = self
            .catalog
            .profiles()
            .iter()
            .filter(|profile| profile.id != QUALITY_ID && profile.id != DYNAMIC_DOMAIN_EXPERT_ID)
            .map(|profile| {
                score_profile(
                    profile,
                    &sources,
                    &input.changed_paths,
                    &languages,
                    &tags,
                    &input.lessons,
                )
            })
            .collect();

        // Stable sort: equal scores keep frozen catalog order.
        scores.sort_by(|a, b| b.score.total_cmp(&a.score));

        let selected_specialists: Vec<String> = scores
            .iter()
            .filter(|score| score.score >= self.threshold)
            .take(MAX_SELECTED_SPECIALISTS)
            .map(|score| score.reviewer_id.clone())
            .collect();
        let dynamic_fallback = selected_specialists.is_empty();

        RoutingDecision {
            scores,
            selected_specialists,
            dynamic_fallback,
            threshold: self.threshold,
        }
    }
}

/// Score one static specialist profile against the input signals.
fn score_profile(
    profile: &crate::domain::reviewer::ReviewerProfile,
    sources: &[Vec<String>],
    changed_paths: &[String],
    languages: &BTreeSet<String>,
    tags: &BTreeSet<String>,
    lessons: &[RoutingHint],
) -> RoutingScore {
    let mut matched_terms = BTreeSet::new();
    for term in &profile.trigger_terms {
        let term_tokens = normalized_tokens(term);
        if sources
            .iter()
            .any(|source| phrase_matches(source, &term_tokens))
        {
            matched_terms.insert(term.clone());
        }
    }

    let matched_paths: BTreeSet<String> = changed_paths
        .iter()
        .filter(|path| {
            profile
                .path_hints
                .iter()
                .any(|pattern| glob_match(pattern, path))
        })
        .cloned()
        .collect();
    let matched_languages: BTreeSet<String> = profile
        .language_hints
        .iter()
        .filter(|hint| languages.contains(*hint))
        .cloned()
        .collect();
    let matched_tags: BTreeSet<String> = profile
        .practice_tags
        .iter()
        .filter(|tag| tags.contains(*tag))
        .cloned()
        .collect();

    let lesson_boost = lesson_boost_for(&profile.id, changed_paths, tags, lessons);
    let trigger_signal = (matched_terms.len() as f64 / TRIGGER_SATURATION_TERMS as f64).min(1.0);
    let path_signal = if matched_paths.is_empty() { 0.0 } else { 1.0 };
    let language_signal = if matched_languages.is_empty() {
        0.0
    } else {
        1.0
    };
    let tag_signal = if matched_tags.is_empty() { 0.0 } else { 1.0 };
    let score = (TRIGGER_WEIGHT * trigger_signal
        + PATH_WEIGHT * path_signal
        + LANGUAGE_WEIGHT * language_signal
        + TAG_WEIGHT * tag_signal
        + lesson_boost)
        .min(1.0);

    RoutingScore {
        reviewer_id: profile.id.clone(),
        score,
        matched_trigger_terms: matched_terms.into_iter().collect(),
        matched_paths: matched_paths.into_iter().collect(),
        matched_languages: matched_languages.into_iter().collect(),
        matched_tags: matched_tags.into_iter().collect(),
        lesson_boost,
    }
}

/// Total lesson boost for one reviewer, capped at the maximum lesson
/// routing boost (plan section 51).
///
/// A hint applies when it names this reviewer, its path globs (when
/// present) match some changed path, and its required tags (when
/// present) intersect the deterministic tags. Boost values outside
/// `[0.0, MAX_ROUTING_BOOST]` contribute nothing: lessons may never
/// suppress or corrupt a score.
fn lesson_boost_for(
    reviewer_id: &str,
    changed_paths: &[String],
    tags: &BTreeSet<String>,
    lessons: &[RoutingHint],
) -> f64 {
    lessons
        .iter()
        .filter(|hint| {
            hint.reviewer_id == reviewer_id && lesson_hint_applies(hint, changed_paths, tags)
        })
        .map(|hint| {
            if (0.0..=MAX_ROUTING_BOOST).contains(&hint.boost) {
                hint.boost
            } else {
                0.0
            }
        })
        .sum::<f64>()
        .min(MAX_ROUTING_BOOST)
}

/// Whether a lesson's routing-hint conditions match this change.
fn lesson_hint_applies(
    hint: &RoutingHint,
    changed_paths: &[String],
    tags: &BTreeSet<String>,
) -> bool {
    if !hint.path_globs.is_empty()
        && !hint
            .path_globs
            .iter()
            .any(|pattern| changed_paths.iter().any(|path| glob_match(pattern, path)))
    {
        return false;
    }
    if !hint.required_tags_any.is_empty()
        && !hint
            .required_tags_any
            .iter()
            .any(|tag| tags.contains(&tag.to_lowercase()))
    {
        return false;
    }
    true
}

/// Normalize text for deterministic matching: lowercase, then turn
/// every non-alphanumeric character (including `-`, `_`, and
/// punctuation) into a space, then split on whitespace. Hyphenated
/// trigger terms therefore match `shared state`, `shared-state`, and
/// `shared_state` alike, and tokens adjacent to punctuation (`queue;`)
/// still match the bare term (`queue`).
fn normalized_tokens(text: &str) -> Vec<String> {
    text.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// Whether the needle's token phrase occurs in the haystack at word
/// boundaries. Empty needles never match.
fn phrase_matches(haystack: &[String], needle: &[impl AsRef<str>]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|window| {
        window
            .iter()
            .zip(needle)
            .all(|(token, needle_token)| token == needle_token.as_ref())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::records::{LessonRecord, RoutingHint};
    use crate::domain::reviewer::ReviewerProfile;
    use crate::jury::plan::build_jury_plan;

    fn catalog() -> Catalog {
        Catalog::bundled().expect("bundled catalog must load")
    }

    fn classifier() -> RulesClassifier {
        RulesClassifier::new(catalog())
    }

    fn hint(
        reviewer_id: &str,
        path_globs: &[&str],
        required_tags_any: &[&str],
        boost: f64,
    ) -> RoutingHint {
        RoutingHint {
            reviewer_id: reviewer_id.to_string(),
            path_globs: path_globs.iter().map(|g| g.to_string()).collect(),
            required_tags_any: required_tags_any.iter().map(|t| t.to_string()).collect(),
            boost,
        }
    }

    fn lesson_with(hint: RoutingHint) -> LessonRecord {
        let mut record = crate::domain::records::testutil::lesson("alice");
        record.routing_hint = hint;
        record
    }

    fn selected_ids(decision: &RoutingDecision) -> Vec<&str> {
        decision
            .selected_specialists
            .iter()
            .map(String::as_str)
            .collect()
    }

    fn score_of<'a>(decision: &'a RoutingDecision, reviewer_id: &str) -> &'a RoutingScore {
        decision
            .scores
            .iter()
            .find(|score| score.reviewer_id == reviewer_id)
            .unwrap_or_else(|| panic!("no score for `{reviewer_id}`"))
    }

    #[test]
    fn normalized_tokens_split_separators_and_boundaries() {
        assert_eq!(
            normalized_tokens("ISR_Handler writes Shared-State."),
            vec!["isr", "handler", "writes", "shared", "state"]
        );
        // Punctuation adjacent to a token does not break matching.
        assert_eq!(
            normalized_tokens("the shared queue; the main loop"),
            vec!["the", "shared", "queue", "the", "main", "loop"]
        );
        assert_eq!(normalized_tokens(""), Vec::<String>::new());
        assert!(phrase_matches(
            &normalized_tokens("uses shared-state guards"),
            &["shared", "state"]
        ));
        assert!(phrase_matches(
            &normalized_tokens("uses shared_state guards"),
            &["shared", "state"]
        ));
        assert!(!phrase_matches(
            &normalized_tokens("shared states differ"),
            &["shared", "state"]
        ));
        // Word boundaries: `db` must not match inside `database`.
        assert!(!phrase_matches(
            &normalized_tokens("database migration"),
            &["db"]
        ));
        // `auth` must not match inside `oauth`.
        assert!(!phrase_matches(&normalized_tokens("oauth"), &["auth"]));
    }

    #[test]
    fn empty_input_falls_back_to_the_dynamic_expert() {
        let decision = classifier().classify(&RoutingInput::empty());
        assert!(decision.selected_specialists.is_empty());
        assert!(decision.dynamic_fallback);
        assert_eq!(decision.threshold, DEFAULT_THRESHOLD);
        for score in &decision.scores {
            assert_eq!(
                score.score, 0.0,
                "{} scored without signals",
                score.reviewer_id
            );
        }
        // Every static specialist is scored exactly once.
        assert_eq!(decision.scores.len(), catalog().profiles().len() - 2);

        let plan = build_jury_plan(&decision, &catalog()).unwrap();
        assert_eq!(
            plan.reviewer_ids(),
            vec!["quality", "dynamic-domain-expert"]
        );
        assert!(plan.uses_dynamic_fallback());
    }

    #[test]
    fn single_weak_signal_stays_below_the_threshold() {
        let input = RoutingInput {
            descriptors: vec!["Handle queue backpressure".to_string()],
            ..RoutingInput::default()
        };
        let decision = classifier().classify(&input);
        let concurrency = score_of(&decision, "concurrency");
        // One trigger term, no language/path/tag signals.
        assert_eq!(concurrency.matched_trigger_terms, vec!["queue".to_string()]);
        let expected = TRIGGER_WEIGHT * (1.0 / TRIGGER_SATURATION_TERMS as f64);
        assert!((concurrency.score - expected).abs() < 1e-12);
        assert!(concurrency.score < DEFAULT_THRESHOLD);
        assert!(decision.dynamic_fallback);
    }

    #[test]
    fn lesson_boost_lifts_a_weak_signal_over_the_threshold() {
        let mut input = RoutingInput {
            descriptors: vec!["Handle queue backpressure".to_string()],
            changed_paths: vec!["src/queue/mod.rs".to_string()],
            deterministic_tags: vec!["queue".to_string()],
            ..RoutingInput::default()
        };
        let without = classifier().classify(&input);
        assert!(without.dynamic_fallback);

        input.lessons = vec![hint("concurrency", &["src/queue/**"], &["queue"], 0.25)];
        let with = classifier().classify(&input);
        assert_eq!(selected_ids(&with), vec!["concurrency"]);
        assert!(!with.dynamic_fallback);
        let boosted = score_of(&with, "concurrency");
        assert_eq!(boosted.lesson_boost, 0.25);
        assert!(boosted.score >= DEFAULT_THRESHOLD);

        // The boost is recorded and never changes other reviewers.
        for score in &with.scores {
            if score.reviewer_id != "concurrency" {
                assert_eq!(score.lesson_boost, 0.0);
            }
        }
    }

    #[test]
    fn lesson_boost_conditions_gate_the_boost() {
        let base = RoutingInput {
            descriptors: vec!["Handle queue backpressure".to_string()],
            changed_paths: vec!["src/queue/mod.rs".to_string()],
            deterministic_tags: vec!["queue".to_string()],
            ..RoutingInput::default()
        };

        // Path glob does not match any changed path.
        let mismatched_path = hint("concurrency", &["src/other/**"], &[], 0.25);
        let input = RoutingInput {
            lessons: vec![mismatched_path],
            ..base.clone()
        };
        assert_eq!(
            score_of(&classifier().classify(&input), "concurrency").lesson_boost,
            0.0
        );

        // Required tag not present.
        let mismatched_tag = hint("concurrency", &[], &["crypto"], 0.25);
        let input = RoutingInput {
            lessons: vec![mismatched_tag],
            ..base.clone()
        };
        assert_eq!(
            score_of(&classifier().classify(&input), "concurrency").lesson_boost,
            0.0
        );

        // Empty globs and empty tags apply unconditionally.
        let unconditional = hint("concurrency", &[], &[], 0.25);
        let input = RoutingInput {
            lessons: vec![unconditional],
            ..base
        };
        assert_eq!(
            score_of(&classifier().classify(&input), "concurrency").lesson_boost,
            0.25
        );
    }

    #[test]
    fn lesson_boosts_are_capped_and_invalid_boosts_are_ignored() {
        let base = RoutingInput {
            descriptors: vec!["Handle queue backpressure".to_string()],
            ..RoutingInput::default()
        };
        let plain = score_of(&classifier().classify(&base), "concurrency").score;

        // Two valid boosts sum past the cap; the total is capped.
        let input = RoutingInput {
            lessons: vec![
                hint("concurrency", &[], &[], MAX_ROUTING_BOOST),
                hint("concurrency", &[], &[], MAX_ROUTING_BOOST),
            ],
            ..base.clone()
        };
        let capped_decision = classifier().classify(&input);
        let capped = score_of(&capped_decision, "concurrency");
        assert_eq!(capped.lesson_boost, MAX_ROUTING_BOOST);

        // Out-of-range and non-finite boosts contribute nothing.
        for bad in [-0.5, 0.9, f64::NAN, f64::INFINITY] {
            let input = RoutingInput {
                lessons: vec![hint("concurrency", &[], &[], bad)],
                ..base.clone()
            };
            let decision = classifier().classify(&input);
            let score = score_of(&decision, "concurrency");
            assert_eq!(score.lesson_boost, 0.0, "boost {bad} must be ignored");
            assert_eq!(
                score.score, plain,
                "invalid boosts must not change the score"
            );
        }
    }

    #[test]
    fn lessons_never_suppress_a_reviewer() {
        let input = RoutingInput {
            descriptors: vec!["Fix race on the shared queue".to_string()],
            languages: vec!["rust".to_string()],
            deterministic_tags: vec!["concurrency".to_string()],
            ..RoutingInput::default()
        };
        let without = classifier().classify(&input);
        let with_other = RoutingInput {
            // Boosts for other reviewers may add them to the jury, but
            // may never lower an existing score.
            lessons: vec![
                hint("database", &[], &[], 0.35),
                hint("security", &[], &[], 0.35),
            ],
            ..input
        };
        let with_other = classifier().classify(&with_other);
        for score in &without.scores {
            let after = score_of(&with_other, &score.reviewer_id);
            assert!(
                after.score >= score.score,
                "lesson for another reviewer suppressed `{}`",
                score.reviewer_id
            );
        }
        // The previously selected reviewer stays selected; boosted
        // reviewers may join; selection remains bounded.
        assert!(
            with_other
                .selected_specialists
                .contains(&"concurrency".to_string())
        );
        assert!(with_other.selected_specialists.len() <= 3);
    }

    #[test]
    fn at_most_three_specialists_are_selected() {
        let input = RoutingInput {
            descriptors: vec![
                "Fix race on the shared queue; optimize hot copy path; migrate the users table for auth token validation"
                    .to_string(),
            ],
            changed_paths: vec![
                "db/migrations/0010_users.sql".to_string(),
                "src/auth/login.rs".to_string(),
                "src/queue/mod.rs".to_string(),
            ],
            languages: vec!["rust".to_string()],
            changed_symbols: vec!["enqueue".to_string(), "render_tile".to_string()],
            deterministic_tags: Vec::new(),
            lessons: Vec::new(),
        };
        let decision = classifier().classify(&input);
        let security = score_of(&decision, "security");
        let concurrency = score_of(&decision, "concurrency");
        let database = score_of(&decision, "database");
        let performance = score_of(&decision, "performance");

        assert!(security.score > concurrency.score);
        // Two trigger terms (race, queue — punctuation-insensitive)
        // plus the language signal.
        let concurrency_expected = TRIGGER_WEIGHT * (2.0 / 3.0) + LANGUAGE_WEIGHT;
        assert!((concurrency.score - concurrency_expected).abs() < 1e-12);
        // One trigger term (table) plus the path signal.
        let database_expected = TRIGGER_WEIGHT * (1.0 / 3.0) + PATH_WEIGHT;
        assert!((database.score - database_expected).abs() < 1e-12);
        assert!(concurrency.score > database.score);

        // All four qualify; only three join the jury, best first.
        assert!(database.score >= DEFAULT_THRESHOLD);
        assert!(performance.score >= DEFAULT_THRESHOLD);
        assert_eq!(
            selected_ids(&decision),
            vec!["security", "concurrency", "performance"]
        );
        assert!(
            !decision
                .selected_specialists
                .contains(&"database".to_string())
        );

        let plan = build_jury_plan(&decision, &catalog()).unwrap();
        assert_eq!(plan.size(), 4);
        assert_eq!(
            plan.reviewer_ids(),
            vec!["quality", "security", "concurrency", "performance"]
        );
    }

    #[test]
    fn selection_uses_the_threshold_inclusively() {
        // Only a language signal: exactly 0.2 for reviewers hinting at
        // that language.
        let input = RoutingInput {
            languages: vec!["c".to_string()],
            ..RoutingInput::default()
        };
        let decision = RulesClassifier::new(catalog())
            .with_threshold(LANGUAGE_WEIGHT)
            .classify(&input);
        // `>= threshold` selects reviewers scoring exactly at the
        // threshold; ties keep catalog order and three are taken.
        assert_eq!(
            selected_ids(&decision),
            vec!["concurrency", "embedded-resource", "realtime"]
        );

        // Above every score: fallback fires.
        let decision = RulesClassifier::new(catalog())
            .with_threshold(0.99)
            .classify(&input);
        assert!(decision.dynamic_fallback);
    }

    #[test]
    fn classify_is_deterministic() {
        let input = RoutingInput {
            descriptors: vec!["Add token validation to the auth flow".to_string()],
            changed_paths: vec!["src/auth/token.rs".to_string()],
            languages: vec!["rust".to_string()],
            changed_symbols: vec!["validate_token".to_string()],
            deterministic_tags: vec!["security".to_string()],
            lessons: vec![hint("security", &["src/**"], &["security"], 0.1)],
        };
        let first = classifier().classify(&input);
        let second = classifier().classify(&input);
        assert_eq!(first, second);
        assert_eq!(
            build_jury_plan(&first, &catalog()).unwrap(),
            build_jury_plan(&second, &catalog()).unwrap()
        );
    }

    #[test]
    fn structural_reviewers_are_never_scored() {
        let decision = classifier().classify(&RoutingInput::empty());
        let scored: BTreeSet<&str> = decision
            .scores
            .iter()
            .map(|score| score.reviewer_id.as_str())
            .collect();
        assert!(!scored.contains(QUALITY_ID));
        assert!(!scored.contains(DYNAMIC_DOMAIN_EXPERT_ID));
    }

    #[test]
    fn lesson_records_can_supply_hints_directly() {
        // Milestone 20 feeds active lessons into routing; the hint is
        // the only lesson field routing reads.
        let record = lesson_with(hint("concurrency", &["src/queue/**"], &[], 0.3));
        let input = RoutingInput {
            descriptors: vec!["Handle queue backpressure".to_string()],
            changed_paths: vec!["src/queue/mod.rs".to_string()],
            lessons: vec![record.routing_hint.clone()],
            ..RoutingInput::default()
        };
        let decision = classifier().classify(&input);
        assert_eq!(score_of(&decision, "concurrency").lesson_boost, 0.3);
        assert_eq!(selected_ids(&decision), vec!["concurrency"]);
    }

    #[test]
    fn weights_sum_to_one() {
        let total = TRIGGER_WEIGHT + PATH_WEIGHT + LANGUAGE_WEIGHT + TAG_WEIGHT;
        assert!((total - 1.0).abs() < 1e-12);
    }

    #[test]
    fn profiles_with_every_signal_reach_a_bounded_maximum() {
        let profile = ReviewerProfile {
            id: "saturated".to_string(),
            display_name: "Saturated".to_string(),
            description: "Test profile.".to_string(),
            trigger_terms: vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()],
            language_hints: vec!["rust".to_string()],
            path_hints: vec!["src/**".to_string()],
            practice_tags: vec!["quality".to_string()],
            checklist: vec!["Check.".to_string()],
            base_instructions: "Work the checklist.".to_string(),
            default_model_profile: "standard".to_string(),
            default_thinking_level: crate::domain::model::ThinkingLevel::Medium,
            allowed_capabilities: crate::domain::reviewer::ReviewerCapability::ALL.to_vec(),
        };
        let sources = vec![normalized_tokens("alpha beta gamma")];
        let languages: BTreeSet<String> = ["rust".to_string()].into();
        let tags: BTreeSet<String> = ["quality".to_string()].into();
        let lessons = vec![hint("saturated", &[], &[], MAX_ROUTING_BOOST)];
        let score = score_profile(
            &profile,
            &sources,
            &["src/a.rs".to_string()],
            &languages,
            &tags,
            &lessons,
        );
        assert_eq!(score.score, 1.0, "score must be capped at 1.0");
        assert_eq!(score.lesson_boost, MAX_ROUTING_BOOST);
    }
}
