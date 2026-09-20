//! Routing evaluation (plan section 69, milestone 23).
//!
//! `nisse eval routing` runs the frozen routing fixtures
//! ([`routing_fixtures`](super::fixtures::routing_fixtures), shared with
//! the routing tests) through the real classification and jury
//! construction pipeline and measures the frozen metrics:
//!
//! - **required reviewer recall** — selected `expected_required`
//!   reviewers over all expected required reviewers;
//! - **forbidden reviewer rate** — selected `expected_forbidden`
//!   reviewers over all selected jury seats;
//! - **average jury size**;
//! - **dynamic-domain fallback rate**;
//! - **latency** (classification plus jury construction per fixture).
//!
//! The mandatory rules baseline variant always runs. When an optional
//! routing provider (plan section 34, the Needle classifier) is
//! configured in trusted local configuration, the same fixtures run
//! again through the provider-blended decision so the two variants can
//! be compared. The provider can only ever *improve* optional
//! behavior: jury constraints are enforced deterministically and
//! provider failures fall back to the rules decision. The command's
//! exit status therefore follows the mandatory rules baseline only;
//! the provider variant is reported for comparison.
//!
//! Evaluation data is recorded in the machine-readable report (the
//! `machine-readable:` section of the rendered output). Nothing trains
//! models (plan section 69).

use std::time::{Duration, Instant};

use serde::Serialize;
use thiserror::Error;

use super::fixtures::{RoutingFixture, routing_fixtures, verify_fixture};
use crate::jury::catalog::Catalog;
use crate::jury::plan::build_jury_plan;
use crate::providers::RoutingClassifierProvider;
use crate::providers::routing::classify_with_optional_provider;
use crate::routing::rules::RulesClassifier;

/// Label of the mandatory rules baseline variant.
pub const RULES_VARIANT: &str = "rules";
/// Label of the optional provider comparison variant.
pub const PROVIDER_VARIANT: &str = "rules+provider";

/// Routing evaluation failures.
#[derive(Debug, Error)]
pub enum RoutingEvalError {
    /// The bundled reviewer catalog failed to load.
    #[error("the bundled reviewer catalog is invalid: {0}")]
    Catalog(String),
    /// Jury construction failed. The frozen jury constraints must hold
    /// for every fixture and every provider blend; a violation here is
    /// a defect, not an evaluation result.
    #[error("jury construction failed for fixture `{fixture}`: {message}")]
    InvalidJury {
        /// Fixture that failed jury construction.
        fixture: String,
        /// Failure detail.
        message: String,
    },
}

/// One fixture's measured routing outcome.
#[derive(Debug, Clone)]
pub struct RoutingFixtureOutcome {
    /// Fixture name.
    pub name: String,
    /// Selected jury, quality first.
    pub jury: Vec<String>,
    /// Jury size (2..=4).
    pub jury_size: usize,
    /// How many of the fixture's expected required reviewers hold
    /// seats.
    pub required_found: usize,
    /// How many reviewers the fixture expects to be required.
    pub required_total: usize,
    /// How many of the fixture's expected forbidden reviewers were
    /// selected (must be zero).
    pub forbidden_selected: usize,
    /// Whether the dynamic-domain-expert fallback fired.
    pub dynamic_fallback: bool,
    /// Classification plus jury-construction latency.
    pub latency: Duration,
    /// First fixture violation, `None` when the fixture passed.
    pub violation: Option<String>,
}

impl RoutingFixtureOutcome {
    /// Whether the jury satisfied the fixture.
    pub fn passed(&self) -> bool {
        self.violation.is_none()
    }
}

/// Aggregate report of one evaluation variant.
#[derive(Debug, Clone)]
pub struct RoutingVariantReport {
    /// Variant label ([`RULES_VARIANT`] or [`PROVIDER_VARIANT`]).
    pub label: &'static str,
    /// Provider name of the comparison variant; `None` for the
    /// mandatory rules baseline.
    pub provider: Option<String>,
    /// One outcome per fixture, frozen fixture order.
    pub outcomes: Vec<RoutingFixtureOutcome>,
    /// Provider fallback diagnostics observed while running the
    /// variant (deduplicated, sorted).
    pub diagnostics: Vec<String>,
}

impl RoutingVariantReport {
    /// Required reviewer recall: selected required reviewers over all
    /// expected required reviewers across all fixtures.
    pub fn required_reviewer_recall(&self) -> f64 {
        let total: usize = self.outcomes.iter().map(|o| o.required_total).sum();
        if total == 0 {
            return 1.0;
        }
        let found: usize = self.outcomes.iter().map(|o| o.required_found).sum();
        found as f64 / total as f64
    }

    /// Forbidden reviewer rate: selected forbidden reviewers over all
    /// selected jury seats across all fixtures.
    pub fn forbidden_reviewer_rate(&self) -> f64 {
        let seats: usize = self.outcomes.iter().map(|o| o.jury_size).sum();
        if seats == 0 {
            return 0.0;
        }
        let forbidden: usize = self.outcomes.iter().map(|o| o.forbidden_selected).sum();
        forbidden as f64 / seats as f64
    }

    /// Average jury size across all fixtures.
    pub fn average_jury_size(&self) -> f64 {
        if self.outcomes.is_empty() {
            return 0.0;
        }
        let sizes: usize = self.outcomes.iter().map(|o| o.jury_size).sum();
        sizes as f64 / self.outcomes.len() as f64
    }

    /// Dynamic-domain fallback rate across all fixtures.
    pub fn dynamic_fallback_rate(&self) -> f64 {
        if self.outcomes.is_empty() {
            return 0.0;
        }
        let fallbacks = self.outcomes.iter().filter(|o| o.dynamic_fallback).count();
        fallbacks as f64 / self.outcomes.len() as f64
    }

    /// Total classification latency across all fixtures.
    pub fn latency_total(&self) -> Duration {
        self.outcomes.iter().map(|o| o.latency).sum()
    }

    /// Mean classification latency per fixture.
    pub fn latency_mean(&self) -> Duration {
        if self.outcomes.is_empty() {
            return Duration::ZERO;
        }
        self.latency_total() / self.outcomes.len() as u32
    }

    /// Number of passed fixtures.
    pub fn fixtures_passed(&self) -> usize {
        self.outcomes.iter().filter(|o| o.passed()).count()
    }

    /// Whether every fixture passed.
    pub fn passed(&self) -> bool {
        self.outcomes.iter().all(|o| o.passed())
    }

    /// Whether this is the mandatory rules baseline whose failure
    /// fails the command.
    pub fn mandatory(&self) -> bool {
        self.provider.is_none()
    }
}

/// Run the frozen routing fixtures.
///
/// The mandatory rules baseline variant is always produced. When
/// `provider` is configured, a second variant runs the same fixtures
/// through the provider-blended decision for comparison.
pub fn run_routing_eval(
    provider: Option<&dyn RoutingClassifierProvider>,
) -> Result<Vec<RoutingVariantReport>, RoutingEvalError> {
    let catalog = Catalog::bundled().map_err(|err| RoutingEvalError::Catalog(err.to_string()))?;
    let fixtures = routing_fixtures();

    let mut variants = Vec::with_capacity(2);
    variants.push(run_variant(&catalog, &fixtures, None)?);
    if let Some(provider) = provider {
        variants.push(run_variant(&catalog, &fixtures, Some(provider))?);
    }
    Ok(variants)
}

/// Run every fixture through one variant and aggregate the metrics.
fn run_variant(
    catalog: &Catalog,
    fixtures: &[RoutingFixture],
    provider: Option<&dyn RoutingClassifierProvider>,
) -> Result<RoutingVariantReport, RoutingEvalError> {
    let classifier = RulesClassifier::new(catalog.clone());
    let mut outcomes = Vec::with_capacity(fixtures.len());
    let mut diagnostics: Vec<String> = Vec::new();

    for fixture in fixtures {
        let outcome = run_fixture(catalog, &classifier, fixture, provider, &mut diagnostics)?;
        outcomes.push(outcome);
    }

    diagnostics.sort();
    diagnostics.dedup();
    Ok(RoutingVariantReport {
        label: if provider.is_some() {
            PROVIDER_VARIANT
        } else {
            RULES_VARIANT
        },
        provider: provider.map(|provider| provider.name().to_string()),
        outcomes,
        diagnostics,
    })
}

/// Classify one fixture, build its jury, and measure the outcome.
fn run_fixture(
    catalog: &Catalog,
    classifier: &RulesClassifier,
    fixture: &RoutingFixture,
    provider: Option<&dyn RoutingClassifierProvider>,
    diagnostics: &mut Vec<String>,
) -> Result<RoutingFixtureOutcome, RoutingEvalError> {
    let start = Instant::now();
    let decision = classifier.classify(&fixture.input);
    let (decision, provider_diagnostics) = match provider {
        Some(provider) => {
            classify_with_optional_provider(decision, Some(provider), &fixture.input, catalog)
        }
        None => (decision, Vec::new()),
    };
    let plan =
        build_jury_plan(&decision, catalog).map_err(|err| RoutingEvalError::InvalidJury {
            fixture: fixture.name.to_string(),
            message: err.to_string(),
        })?;
    let latency = start.elapsed();

    diagnostics.extend(provider_diagnostics);
    let jury: Vec<String> = plan
        .reviewer_ids()
        .iter()
        .map(|id| id.to_string())
        .collect();
    let violation = verify_fixture(&plan, fixture).err();

    Ok(RoutingFixtureOutcome {
        name: fixture.name.to_string(),
        required_found: fixture
            .expected_required_reviewers
            .iter()
            .filter(|required| plan.contains(required))
            .count(),
        required_total: fixture.expected_required_reviewers.len(),
        forbidden_selected: fixture
            .expected_forbidden_reviewers
            .iter()
            .filter(|forbidden| plan.contains(forbidden))
            .count(),
        jury_size: plan.size(),
        dynamic_fallback: plan.uses_dynamic_fallback(),
        jury,
        latency,
        violation,
    })
}

/// Render the complete routing evaluation report: a human-readable
/// summary followed by one machine-readable JSON line after the
/// `machine-readable:` marker.
pub fn render_routing_report(variants: &[RoutingVariantReport]) -> String {
    let mut out = String::from("nisse eval routing\n");
    for variant in variants {
        out.push_str(&format!(
            "variant: {} ({})\n",
            variant.label,
            match &variant.provider {
                Some(provider) => format!("provider: {provider}"),
                None => "mandatory baseline".to_string(),
            }
        ));
        for outcome in &variant.outcomes {
            let verdict = if outcome.passed() { "pass" } else { "FAIL" };
            out.push_str(&format!(
                "  {verdict}  {:<24} jury: {}\n",
                outcome.name,
                outcome.jury.join(", ")
            ));
            if let Some(violation) = &outcome.violation {
                out.push_str(&format!("        {violation}\n"));
            }
        }
        out.push_str(&format!(
            "  required reviewer recall: {:.3} ({}/{})\n",
            variant.required_reviewer_recall(),
            variant
                .outcomes
                .iter()
                .map(|o| o.required_found)
                .sum::<usize>(),
            variant
                .outcomes
                .iter()
                .map(|o| o.required_total)
                .sum::<usize>(),
        ));
        out.push_str(&format!(
            "  forbidden reviewer rate: {:.3}\n",
            variant.forbidden_reviewer_rate()
        ));
        out.push_str(&format!(
            "  average jury size: {:.2}\n",
            variant.average_jury_size()
        ));
        out.push_str(&format!(
            "  dynamic-domain fallback rate: {:.3} ({}/{})\n",
            variant.dynamic_fallback_rate(),
            variant
                .outcomes
                .iter()
                .filter(|o| o.dynamic_fallback)
                .count(),
            variant.outcomes.len()
        ));
        out.push_str(&format!(
            "  latency: mean {:.3} ms / total {:.3} ms\n",
            duration_ms(variant.latency_mean()),
            duration_ms(variant.latency_total())
        ));
        for diagnostic in &variant.diagnostics {
            out.push_str(&format!("  note: {diagnostic}\n"));
        }
        out.push_str(&format!(
            "  verdict: {}{}\n",
            if variant.passed() { "pass" } else { "fail" },
            if variant.mandatory() {
                String::new()
            } else {
                " (comparison only; exit status follows the mandatory baseline)".to_string()
            }
        ));
    }
    out.push_str("\nmachine-readable:\n");
    out.push_str(&render_routing_json(variants));
    out.push('\n');
    out
}

/// The machine-readable JSON form of the report (one compact line).
pub fn render_routing_json(variants: &[RoutingVariantReport]) -> String {
    let report = JsonReport {
        evaluation: "routing",
        variants: variants
            .iter()
            .map(|variant| JsonVariant {
                label: variant.label,
                provider: variant.provider.as_deref(),
                mandatory: variant.mandatory(),
                required_reviewer_recall: round3(variant.required_reviewer_recall()),
                forbidden_reviewer_rate: round3(variant.forbidden_reviewer_rate()),
                average_jury_size: round3(variant.average_jury_size()),
                dynamic_domain_fallback_rate: round3(variant.dynamic_fallback_rate()),
                fixtures_passed: variant.fixtures_passed(),
                fixtures_total: variant.outcomes.len(),
                latency_mean_ms: duration_ms(variant.latency_mean()),
                latency_total_ms: duration_ms(variant.latency_total()),
                passed: variant.passed(),
                diagnostics: &variant.diagnostics,
                fixtures: variant
                    .outcomes
                    .iter()
                    .map(|outcome| JsonFixture {
                        name: &outcome.name,
                        jury: &outcome.jury,
                        jury_size: outcome.jury_size,
                        required_found: outcome.required_found,
                        required_total: outcome.required_total,
                        forbidden_selected: outcome.forbidden_selected,
                        dynamic_fallback: outcome.dynamic_fallback,
                        latency_ms: duration_ms(outcome.latency),
                        passed: outcome.passed(),
                        violation: outcome.violation.as_deref(),
                    })
                    .collect(),
            })
            .collect(),
    };
    serde_json::to_string(&report).expect("evaluation reports always serialize")
}

/// Milliseconds with microsecond resolution.
fn duration_ms(duration: Duration) -> f64 {
    round3(duration.as_secs_f64() * 1000.0)
}

/// Round to three decimals so reports are stable to the eye.
fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

#[derive(Serialize)]
struct JsonReport<'a> {
    evaluation: &'static str,
    variants: Vec<JsonVariant<'a>>,
}

#[derive(Serialize)]
struct JsonVariant<'a> {
    label: &'a str,
    provider: Option<&'a str>,
    mandatory: bool,
    required_reviewer_recall: f64,
    forbidden_reviewer_rate: f64,
    average_jury_size: f64,
    dynamic_domain_fallback_rate: f64,
    fixtures_passed: usize,
    fixtures_total: usize,
    latency_mean_ms: f64,
    latency_total_ms: f64,
    passed: bool,
    diagnostics: &'a [String],
    fixtures: Vec<JsonFixture<'a>>,
}

#[derive(Serialize)]
struct JsonFixture<'a> {
    name: &'a str,
    jury: &'a [String],
    jury_size: usize,
    required_found: usize,
    required_total: usize,
    forbidden_selected: usize,
    dynamic_fallback: bool,
    latency_ms: f64,
    passed: bool,
    violation: Option<&'a str>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::routing::RoutingInput;
    use crate::providers::ProviderError;

    /// The identity of one outcome, ignoring latency (which varies).
    fn signature(outcome: &RoutingFixtureOutcome) -> String {
        format!(
            "{}|{:?}|{}/{}/{}|{}|{:?}",
            outcome.name,
            outcome.jury,
            outcome.required_found,
            outcome.required_total,
            outcome.forbidden_selected,
            outcome.dynamic_fallback,
            outcome.violation,
        )
    }

    fn variant_signature(variant: &RoutingVariantReport) -> Vec<String> {
        variant.outcomes.iter().map(signature).collect()
    }

    /// A scripted provider for comparison tests.
    struct ScriptedProvider {
        result: Result<Vec<crate::providers::ProviderScore>, ()>,
    }

    impl RoutingClassifierProvider for ScriptedProvider {
        fn name(&self) -> &'static str {
            "scripted"
        }

        fn route(
            &self,
            _input: &RoutingInput,
        ) -> Result<Vec<crate::providers::ProviderScore>, ProviderError> {
            match &self.result {
                Ok(scores) => Ok(scores.clone()),
                Err(()) => Err(ProviderError::Transport {
                    provider: "scripted".to_string(),
                    message: "scripted failure".to_string(),
                }),
            }
        }
    }

    fn score(id: &str, value: f64) -> crate::providers::ProviderScore {
        crate::providers::ProviderScore {
            reviewer_id: id.to_string(),
            score: value,
        }
    }

    #[test]
    fn metric_aggregates_are_computed_correctly() {
        let report = RoutingVariantReport {
            label: RULES_VARIANT,
            provider: None,
            diagnostics: Vec::new(),
            outcomes: vec![
                RoutingFixtureOutcome {
                    name: "a".to_string(),
                    jury: vec!["quality".to_string(), "security".to_string()],
                    jury_size: 2,
                    required_found: 2,
                    required_total: 2,
                    forbidden_selected: 0,
                    dynamic_fallback: false,
                    latency: Duration::from_millis(10),
                    violation: None,
                },
                RoutingFixtureOutcome {
                    name: "b".to_string(),
                    jury: vec![
                        "quality".to_string(),
                        "database".to_string(),
                        "dynamic-domain-expert".to_string(),
                    ],
                    jury_size: 3,
                    required_found: 1,
                    required_total: 2,
                    forbidden_selected: 1,
                    dynamic_fallback: true,
                    latency: Duration::from_millis(30),
                    violation: Some("fixture `b` requires reviewer `testing`".to_string()),
                },
            ],
        };

        // Required recall: (2 + 1) / (2 + 2).
        assert!((report.required_reviewer_recall() - 0.75).abs() < 1e-12);
        // Forbidden rate: 1 forbidden seat over 5 selected seats.
        assert!((report.forbidden_reviewer_rate() - 0.2).abs() < 1e-12);
        // Average jury size: (2 + 3) / 2.
        assert!((report.average_jury_size() - 2.5).abs() < 1e-12);
        // Fallback rate: 1 of 2.
        assert!((report.dynamic_fallback_rate() - 0.5).abs() < 1e-12);
        assert_eq!(report.latency_total(), Duration::from_millis(40));
        assert_eq!(report.latency_mean(), Duration::from_millis(20));
        assert_eq!(report.fixtures_passed(), 1);
        assert!(!report.passed());
        assert!(report.mandatory());
    }

    #[test]
    fn empty_reports_degenerate_safely() {
        let report = RoutingVariantReport {
            label: RULES_VARIANT,
            provider: None,
            diagnostics: Vec::new(),
            outcomes: Vec::new(),
        };
        assert_eq!(report.required_reviewer_recall(), 1.0);
        assert_eq!(report.forbidden_reviewer_rate(), 0.0);
        assert_eq!(report.average_jury_size(), 0.0);
        assert_eq!(report.dynamic_fallback_rate(), 0.0);
        assert_eq!(report.latency_mean(), Duration::ZERO);
        assert!(report.passed());
    }

    #[test]
    fn deterministic_rules_baseline_passes_every_fixture() {
        let first = run_routing_eval(None).unwrap();
        let second = run_routing_eval(None).unwrap();

        assert_eq!(first.len(), 1, "no provider means only the rules variant");
        let baseline = &first[0];
        assert_eq!(baseline.label, RULES_VARIANT);
        assert!(baseline.provider.is_none());
        assert!(baseline.mandatory());
        assert_eq!(
            baseline.outcomes.len(),
            super::super::fixtures::routing_fixtures().len()
        );
        for outcome in &baseline.outcomes {
            assert!(
                outcome.passed(),
                "{}",
                outcome.violation.clone().unwrap_or_default()
            );
            assert_eq!(outcome.forbidden_selected, 0);
            assert!(
                (2..=4).contains(&outcome.jury_size),
                "jury size out of bounds: {:?}",
                outcome.jury
            );
            assert_eq!(outcome.jury.first().map(String::as_str), Some("quality"));
        }
        assert!(baseline.passed());

        // Determinism: identical outcomes on a second run (latency
        // aside).
        assert_eq!(variant_signature(baseline), variant_signature(&second[0]));
    }

    #[test]
    fn provider_variant_is_reported_alongside_the_untouched_baseline() {
        // A provider proposing a specialist for every fixture must not
        // change the mandatory rules baseline, but must produce the
        // comparison variant under the provider's name.
        let provider = ScriptedProvider {
            result: Ok(vec![score("testing", 0.9)]),
        };
        let variants = run_routing_eval(Some(&provider)).unwrap();
        assert_eq!(variants.len(), 2);

        let baseline = &variants[0];
        let comparison = &variants[1];
        assert_eq!(comparison.label, PROVIDER_VARIANT);
        assert_eq!(comparison.provider.as_deref(), Some("scripted"));
        assert!(!comparison.mandatory());
        assert_eq!(
            baseline
                .outcomes
                .iter()
                .map(|o| &o.jury)
                .collect::<Vec<_>>(),
            run_routing_eval(None).unwrap()[0]
                .outcomes
                .iter()
                .map(|o| &o.jury)
                .collect::<Vec<_>>(),
            "the rules baseline must be independent of the provider"
        );

        // The proposal blends into the comparison juries: `testing`
        // reaches the threshold (0.5 * rules + 0.5 * 0.9 >= 0.35) for
        // every fixture where the rules score is 0.
        assert!(
            comparison
                .outcomes
                .iter()
                .any(|outcome| outcome.jury.iter().any(|id| id == "testing"))
        );

        // A failing provider degrades to the rules decision with a
        // recorded diagnostic.
        let failing = ScriptedProvider { result: Err(()) };
        let variants = run_routing_eval(Some(&failing)).unwrap();
        assert_eq!(variants.len(), 2);
        assert_eq!(
            variant_signature(&variants[1]),
            variant_signature(&variants[0]),
            "provider failure must fall back to rules-only routing"
        );
        assert!(
            variants[1]
                .diagnostics
                .iter()
                .any(|message| message.contains("scripted routing unavailable")),
            "{:?}",
            variants[1].diagnostics
        );
    }

    #[test]
    fn report_renders_text_and_machine_readable_json() {
        let variants = run_routing_eval(None).unwrap();
        let report = render_routing_report(&variants);
        assert!(report.starts_with("nisse eval routing\n"));
        assert!(report.contains("variant: rules (mandatory baseline)"));
        assert!(report.contains("verdict: pass"));

        let json = report
            .split("machine-readable:\n")
            .nth(1)
            .expect("machine-readable marker")
            .lines()
            .next()
            .expect("one JSON line");
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(value["evaluation"], "routing");
        let variant = &value["variants"][0];
        assert_eq!(variant["label"], "rules");
        assert_eq!(variant["mandatory"], true);
        assert_eq!(variant["passed"], true);
        assert_eq!(
            variant["fixtures_total"].as_u64().unwrap() as usize,
            variants[0].outcomes.len()
        );
        assert_eq!(variant["fixtures_passed"], variant["fixtures_total"]);
        assert!(variant["required_reviewer_recall"].as_f64().unwrap() >= 0.999);
        assert_eq!(variant["forbidden_reviewer_rate"].as_f64().unwrap(), 0.0);
        assert!(
            variant["fixtures"][0]["jury"]
                .as_array()
                .unwrap()
                .first()
                .unwrap()
                == "quality"
        );
        assert!(variant["fixtures"][0]["latency_ms"].as_f64().unwrap() >= 0.0);
    }
}
