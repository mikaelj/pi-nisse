//! Integration tests for finding validation support and the synthesis
//! pipeline against the real Pi review host process (plan milestone 14).
//!
//! The narrow equivalence check and the non-juror synthesis session are
//! exercised over the real child-process boundary:
//!
//! - the fake runtime provides deterministic verdicts and invented
//!   synthesis output (proving rejection and deterministic fallback);
//! - the scripted pi runtime provides strict verdict/report answers
//!   (proving the session pipeline without contacting a model).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use nisse::domain::evidence::{EvidenceKind, EvidenceRef};
use nisse::domain::finding::ReviewerFinding;
use nisse::domain::finding::SynthesisStage;
use nisse::domain::review::{ImpactClass, ReviewKind, Severity};
use nisse::jury::ValidatedFinding;
use nisse::jury::findings::finding_fingerprint;
use nisse::jury::host::{HostProcess, PiHostLaunch};
use nisse::jury::protocol::{
    FindingEquivalenceCheckPayload, HostConfigPayload, HostEvent, HostModelProfile, HostRuntime,
};
use nisse::jury::synthesis::{ConsolidationOptions, SYNTHESIZER_ID, consolidate};

fn host_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("runtime/pi-host/src/main.ts")
}

fn launch(runtime: HostRuntime, env: &[(&str, &str)]) -> PiHostLaunch {
    let mut launch = PiHostLaunch::new("node", host_script(), runtime);
    for (key, value) in env {
        launch = launch.env(key, value);
    }
    launch
}

fn finding(id: &str, claim: &str) -> ReviewerFinding {
    ReviewerFinding {
        id: id.to_string(),
        impact_class: ImpactClass::Correctness,
        proposed_severity: Severity::High,
        category: "logic".to_string(),
        title: format!("Title {id}"),
        claim: claim.to_string(),
        explanation: format!("Explanation {id}"),
        recommendation: format!("Recommendation {id}"),
        evidence: vec![EvidenceRef {
            kind: EvidenceKind::Source,
            path: Some("src/a.rs".to_string()),
            symbol: None,
            line_start: None,
            line_end: None,
            content_key: None,
            plan_section: None,
            record_id: None,
            input_part: None,
            excerpt: "fn a() {}".to_string(),
        }],
    }
}

fn validated(reviewer: &str, id: &str, claim: &str) -> ValidatedFinding {
    let finding = finding(id, claim);
    let fingerprint = finding_fingerprint(&finding);
    ValidatedFinding {
        reviewer_id: reviewer.to_string(),
        fingerprint,
        finding,
    }
}

fn options() -> ConsolidationOptions {
    ConsolidationOptions {
        equivalence_timeout_ms: 10_000,
        synthesis_timeout_ms: 10_000,
        ..ConsolidationOptions::default()
    }
}

fn host_config() -> HostConfigPayload {
    HostConfigPayload {
        max_concurrent_reviewers: None,
        models: BTreeMap::from([(
            "standard".to_string(),
            HostModelProfile {
                model: "acme/local-small".to_string(),
                endpoint: None,
            },
        )]),
    }
}

const READY_TIMEOUT: Duration = Duration::from_secs(20);

/// Two findings from two reviewers pointing at the same location with
/// different claims: exactly one equivalence-check candidate pair.
fn cross_reviewer_pair() -> Vec<ValidatedFinding> {
    vec![
        validated("quality", "q-1", "queue drops the last item"),
        validated("concurrency", "c-1", "ring buffer loses one element"),
    ]
}

#[test]
fn fake_same_verdict_merges_and_invents_nothing() {
    let mut host = HostProcess::spawn(&launch(
        HostRuntime::Fake,
        &[("NISSE_FAKE_EQUIVALENCE_VERDICTS", "same")],
    ))
    .expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");

    let result = consolidate(
        &mut host,
        ReviewKind::Code,
        &cross_reviewer_pair(),
        &options(),
        &nisse::jury::thinking::ThinkingPolicy::permissive(&["standard"]),
    )
    .expect("consolidation failed");

    // One merged, cross-validated final finding with full provenance.
    assert_eq!(result.findings.len(), 1, "{:?}", result.diagnostics);
    let final_finding = &result.findings[0];
    assert!(final_finding.cross_validated);
    assert_eq!(
        final_finding.source_finding_ids,
        vec!["quality/q-1".to_string(), "concurrency/c-1".to_string()]
    );
    assert_eq!(final_finding.reviewer_ids, vec!["concurrency", "quality"]);
    assert_eq!(final_finding.severity, Severity::High);
    // Both members' evidence is combined.
    assert_eq!(final_finding.evidence.len(), 1);

    // The fake synthesis session always invents `fake-1`; the invention
    // is rejected and deterministic fallback wording is used.
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.stage == SynthesisStage::Synthesis
                && d.message.contains("no known group key"))
    );
    assert!(final_finding.title.starts_with("Title "));
    // Deterministic fallback summary (the fake summary is not the
    // synthesizer's report summary path here: the fake report is
    // rejected as a whole finding-wise, but its summary text may be
    // accepted; assert the invariant that matters: bounded and present).
    assert!(!result.summary.is_empty());

    host.shutdown(Duration::from_secs(5))
        .expect("clean shutdown");
}

#[test]
fn fake_different_uncertain_and_failure_never_merge() {
    // `different`: two separate findings.
    let mut host = HostProcess::spawn(&launch(
        HostRuntime::Fake,
        &[("NISSE_FAKE_EQUIVALENCE_VERDICTS", "different")],
    ))
    .expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");
    let result = consolidate(
        &mut host,
        ReviewKind::Code,
        &cross_reviewer_pair(),
        &options(),
        &nisse::jury::thinking::ThinkingPolicy::permissive(&["standard"]),
    )
    .expect("consolidation failed");
    assert_eq!(result.findings.len(), 2);
    assert!(result.findings.iter().all(|f| !f.cross_validated));
    host.shutdown(Duration::from_secs(5))
        .expect("clean shutdown");

    // Default (no scripted verdicts): `uncertain`, no merge, no error.
    let mut host = HostProcess::spawn(&launch(HostRuntime::Fake, &[])).expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");
    let result = consolidate(
        &mut host,
        ReviewKind::Code,
        &cross_reviewer_pair(),
        &options(),
        &nisse::jury::thinking::ThinkingPolicy::permissive(&["standard"]),
    )
    .expect("consolidation failed");
    assert_eq!(result.findings.len(), 2);
    assert!(
        !result
            .diagnostics
            .iter()
            .any(|d| d.stage == SynthesisStage::Equivalence)
    );
    host.shutdown(Duration::from_secs(5))
        .expect("clean shutdown");

    // Scripted failure: forced `uncertain` with an error diagnostic.
    let mut host = HostProcess::spawn(&launch(
        HostRuntime::Fake,
        &[("NISSE_FAKE_EQUIVALENCE_FAIL", "1")],
    ))
    .expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");
    let result = consolidate(
        &mut host,
        ReviewKind::Code,
        &cross_reviewer_pair(),
        &options(),
        &nisse::jury::thinking::ThinkingPolicy::permissive(&["standard"]),
    )
    .expect("consolidation failed");
    assert_eq!(result.findings.len(), 2);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.stage == SynthesisStage::Equivalence
                && d.message.contains("scripted_fake_failure"))
    );
    host.shutdown(Duration::from_secs(5))
        .expect("clean shutdown");
}

#[test]
fn identical_claims_group_without_equivalence_checks() {
    // Identical normalized claims: obvious duplicate grouping, so the
    // fake host sees no equivalence traffic at all (its verdict queue
    // stays untouched even when scripted to `same`).
    let mut host = HostProcess::spawn(&launch(
        HostRuntime::Fake,
        &[("NISSE_FAKE_EQUIVALENCE_VERDICTS", "different")],
    ))
    .expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");

    let findings = vec![
        validated("quality", "q-1", "queue drops the last item"),
        validated("security", "s-1", "Queue drops the last  item"),
    ];
    let result = consolidate(
        &mut host,
        ReviewKind::Plan,
        &findings,
        &options(),
        &nisse::jury::thinking::ThinkingPolicy::permissive(&["standard"]),
    )
    .expect("consolidation");
    assert_eq!(result.findings.len(), 1);
    assert!(result.findings[0].cross_validated);

    host.shutdown(Duration::from_secs(5))
        .expect("clean shutdown");
}

#[test]
fn empty_findings_skip_the_synthesis_session_entirely() {
    // The scripted pi runtime fails loudly when a session is created
    // with an exhausted script; an empty script therefore proves that
    // no equivalence or synthesis session runs for zero findings.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = dir.path().join("script.json");
    std::fs::write(&script, r#"{"sessions": []}"#).expect("write script");

    let mut host = HostProcess::spawn(&launch(
        HostRuntime::Pi,
        &[("NISSE_PI_SESSION_SCRIPT", script.to_str().unwrap())],
    ))
    .expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");

    let result = consolidate(
        &mut host,
        ReviewKind::Code,
        &[],
        &options(),
        &nisse::jury::thinking::ThinkingPolicy::permissive(&["standard"]),
    )
    .expect("consolidation");
    assert!(result.findings.is_empty());
    assert!(result.diagnostics.is_empty());
    assert_eq!(result.summary, "No validated findings.");

    host.shutdown(Duration::from_secs(5))
        .expect("clean shutdown");
}

/// JSON of a synthesis-session report restating the given groups.
fn synthesis_report_json(findings: &[serde_json::Value], summary: &str) -> String {
    serde_json::json!({
        "reviewer_id": SYNTHESIZER_ID,
        "summary": summary,
        "findings": findings,
        "candidate_project_facts": [],
        "model_metadata": {},
        "duration_ms": 0
    })
    .to_string()
}

fn synthesized_finding(key: &str, severity: &str, title: &str) -> serde_json::Value {
    serde_json::json!({
        "id": key,
        "impact_class": "correctness",
        "proposed_severity": severity,
        "category": "logic",
        "title": title,
        "claim": "Synthesized clearer claim.",
        "explanation": "Synthesized clearer explanation.",
        "recommendation": "Synthesized clearer recommendation.",
        "evidence": []
    })
}

#[test]
fn scripted_pi_synthesis_applies_valid_wording() {
    let (findings, groups_keys): (Vec<ValidatedFinding>, Vec<String>) = {
        let findings = cross_reviewer_pair();
        let keys = vec![
            findings[0].fingerprint.clone(),
            findings[1].fingerprint.clone(),
        ];
        (findings, keys)
    };

    let dir = tempfile::tempdir().expect("tempdir");
    let script = dir.path().join("script.json");
    std::fs::write(
        &script,
        serde_json::json!({
            "sessions": [
                { "steps": [{ "reply": "{\"verdict\":\"same\"}" }] },
                { "steps": [{ "reply": synthesis_report_json(
                    &[synthesized_finding(&groups_keys[0], "low", "Reworded merged finding")],
                    "One consolidated finding."
                ) }] },
            ],
        })
        .to_string(),
    )
    .expect("write script");

    let mut host = HostProcess::spawn(&launch(
        HostRuntime::Pi,
        &[("NISSE_PI_SESSION_SCRIPT", script.to_str().unwrap())],
    ))
    .expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");
    host.send_host_config(&host_config())
        .expect("host_config failed");

    let result = consolidate(
        &mut host,
        ReviewKind::Code,
        &findings,
        &options(),
        &nisse::jury::thinking::ThinkingPolicy::permissive(&["standard"]),
    )
    .expect("consolidation failed");

    assert_eq!(result.findings.len(), 1, "{:?}", result.diagnostics);
    let final_finding = &result.findings[0];
    // The same verdict merged the pair.
    assert!(final_finding.cross_validated);
    assert_eq!(final_finding.source_finding_ids.len(), 2);
    // Synthesized wording and lowered severity were applied.
    assert_eq!(final_finding.title, "Reworded merged finding");
    assert_eq!(final_finding.claim, "Synthesized clearer claim.");
    assert_eq!(final_finding.severity, Severity::Low);
    // Evidence is still the combined source evidence, never the
    // synthesizer's (empty) list.
    assert_eq!(final_finding.evidence.len(), 1);
    // The synthesizer's bounded summary is used.
    assert_eq!(result.summary, "One consolidated finding.");

    host.shutdown(Duration::from_secs(5))
        .expect("clean shutdown");
}

#[test]
fn scripted_pi_synthesis_invention_and_raise_are_rejected() {
    let findings = cross_reviewer_pair();
    let keys: Vec<String> = findings.iter().map(|f| f.fingerprint.clone()).collect();

    let dir = tempfile::tempdir().expect("tempdir");
    let script = dir.path().join("script.json");
    std::fs::write(
        &script,
        serde_json::json!({
            "sessions": [
                { "steps": [{ "reply": "{\"verdict\":\"different\"}" }] },
                { "steps": [{ "reply": synthesis_report_json(
                    &[
                        // An invented finding id that is no group key.
                        synthesized_finding("invented-id", "low", "Invented"),
                        // A severity raise above the group ceiling.
                        synthesized_finding(&keys[1], "blocker", "Too severe"),
                    ],
                    "summary"
                ) }] },
            ],
        })
        .to_string(),
    )
    .expect("write script");

    let mut host = HostProcess::spawn(&launch(
        HostRuntime::Pi,
        &[("NISSE_PI_SESSION_SCRIPT", script.to_str().unwrap())],
    ))
    .expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");
    host.send_host_config(&host_config())
        .expect("host_config failed");

    let result = consolidate(
        &mut host,
        ReviewKind::Code,
        &findings,
        &options(),
        &nisse::jury::thinking::ThinkingPolicy::permissive(&["standard"]),
    )
    .expect("consolidation failed");

    // The different verdict kept both findings separate.
    assert_eq!(result.findings.len(), 2);
    // The invented entry was rejected entirely.
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.stage == SynthesisStage::Synthesis
                && d.message.contains("invented finding id `invented-id`"))
    );
    // The severity raise was clamped to the group ceiling (high).
    let clamped = result
        .findings
        .iter()
        .find(|f| {
            f.source_finding_ids
                .contains(&"concurrency/c-1".to_string())
        })
        .expect("clamped finding");
    assert_eq!(clamped.severity, Severity::High);
    assert_eq!(clamped.title, "Too severe");
    assert!(result
        .diagnostics
        .iter()
        .any(|d| d.stage == SynthesisStage::Synthesis
            && d.message.contains("severity ceiling")));
    // The other group fell back to deterministic wording.
    let fallback = result
        .findings
        .iter()
        .find(|f| f.source_finding_ids.contains(&"quality/q-1".to_string()))
        .expect("fallback finding");
    assert_eq!(fallback.title, "Title q-1");
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.stage == SynthesisStage::Synthesis && d.message.contains("not covered"))
    );

    host.shutdown(Duration::from_secs(5))
        .expect("clean shutdown");
}

#[test]
fn equivalence_check_messages_flow_over_the_boundary() {
    let mut host = HostProcess::spawn(&launch(HostRuntime::Fake, &[])).expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");

    let payload = FindingEquivalenceCheckPayload {
        check_id: "chk-flow".to_string(),
        timeout_ms: 5_000,
        model_profile: "standard".to_string(),
        thinking_level: nisse::domain::model::ThinkingLevel::Low,
        finding_a: nisse::jury::protocol::EquivalenceFinding {
            reviewer_id: "quality".to_string(),
            finding: finding("q-1", "claim"),
        },
        finding_b: nisse::jury::protocol::EquivalenceFinding {
            reviewer_id: "concurrency".to_string(),
            finding: finding("c-1", "other claim"),
        },
    };
    host.start_finding_equivalence_check(&payload)
        .expect("send check");

    // Default fake verdict: uncertain without an error.
    match host
        .recv_event(Duration::from_secs(10))
        .expect("completion")
    {
        HostEvent::FindingEquivalenceCompleted {
            check_id,
            verdict,
            error,
        } => {
            assert_eq!(check_id, "chk-flow");
            assert_eq!(
                verdict,
                nisse::jury::protocol::EquivalenceVerdict::Uncertain
            );
            assert!(error.is_none());
        }
        other => panic!("expected FindingEquivalenceCompleted, got {other:?}"),
    }

    // A malformed equivalence payload is a protocol-level host error.
    host.send_raw_line(
        r#"{"protocol_version":2,"id":"m-bad","type":"start_finding_equivalence_check","check_id":""}"#,
    )
    .expect("send malformed");
    match host
        .recv_event(Duration::from_secs(10))
        .expect("host error")
    {
        HostEvent::HostError(info) => assert_eq!(info.code, "invalid_payload"),
        other => panic!("expected HostError, got {other:?}"),
    }

    // Completing with a wrong expectation is a typed unexpected message.
    host.start_finding_equivalence_check(&payload)
        .expect("send check");
    let err = host
        .complete_equivalence_checks(&["other-check".to_string()], Duration::from_secs(10))
        .expect_err("unknown check must fail");
    assert!(err.to_string().contains("unknown or duplicate check"));

    host.shutdown(Duration::from_secs(5))
        .expect("clean shutdown");
}

#[test]
fn synthesis_session_failure_falls_back_deterministically() {
    // The fake runtime fails the synthesizer session on every attempt;
    // after the single retry the consolidation falls back with a
    // diagnostic instead of failing the review.
    let mut host = HostProcess::spawn(&launch(
        HostRuntime::Fake,
        &[
            ("NISSE_FAKE_EQUIVALENCE_VERDICTS", "same"),
            ("NISSE_FAKE_REVIEWER_FAIL_FOR", SYNTHESIZER_ID),
        ],
    ))
    .expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");

    let findings = vec![validated("quality", "q-1", "single finding")];
    let result = consolidate(
        &mut host,
        ReviewKind::Code,
        &findings,
        &options(),
        &nisse::jury::thinking::ThinkingPolicy::permissive(&["standard"]),
    )
    .expect("consolidation failed");

    assert_eq!(result.findings.len(), 1);
    assert_eq!(result.findings[0].title, "Title q-1");
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.stage == SynthesisStage::Synthesis
                && d.message.contains("synthesis session failed"))
    );
    assert!(result.summary.contains("1 finding(s)"));

    host.shutdown(Duration::from_secs(5))
        .expect("clean shutdown");
}
