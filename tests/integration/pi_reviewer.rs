//! Integration tests for constrained isolated reviewer execution with the
//! Pi review host (plan milestone 12).
//!
//! The pi reviewer runtime is exercised through the scripted mock agent
//! (`NISSE_PI_SESSION_SCRIPT`/`NISSE_PI_SESSION_RECORD`), so these tests
//! verify the full pipeline — spec mapping, isolation, strict schema,
//! malformed-output repair, timeouts, and the Rust-side one-retry policy —
//! without contacting any model provider. The deterministic fake runtime
//! covers the retry policy end to end.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use nisse::domain::context::ContextEntry;
use nisse::domain::model::ThinkingLevel;
use nisse::domain::review::ReviewKind;
use nisse::domain::reviewer::{ReviewerCapability, ReviewerExecutionSpec};
use nisse::jury::execution::execute_review;
use nisse::jury::host::{HostProcess, PiHostLaunch, ReviewerOutcome};
use nisse::jury::protocol::{
    HostConfigPayload, HostModelProfile, HostRuntime, StartReviewPayload, ToolResponsePayload,
};

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

fn spec(reviewer_id: &str, timeout_ms: u64) -> ReviewerExecutionSpec {
    ReviewerExecutionSpec {
        reviewer_id: reviewer_id.to_string(),
        specialization: "general correctness".to_string(),
        instructions: "Work the checklist; report only evidenced findings.".to_string(),
        checklist: vec![
            "Check error paths.".to_string(),
            "Check shared state.".to_string(),
        ],
        model_profile: "standard".to_string(),
        thinking_level: ThinkingLevel::Medium,
        initial_context: vec![ContextEntry {
            kind: nisse::domain::context::ContextKind::SymbolDefinition,
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
        allowed_capabilities: vec![
            ReviewerCapability::ProjectLookup,
            ReviewerCapability::SourceLookup,
        ],
        max_lookup_calls: 4,
        max_returned_context_bytes: 65_536,
        timeout_ms,
    }
}

fn host_config(model: &str, endpoint: Option<&str>) -> HostConfigPayload {
    HostConfigPayload {
        max_concurrent_reviewers: None,
        models: BTreeMap::from([(
            "standard".to_string(),
            HostModelProfile {
                model: model.to_string(),
                endpoint: endpoint.map(str::to_string),
            },
        )]),
    }
}

fn valid_report(reviewer_id: &str) -> String {
    format!(
        r#"{{"reviewer_id":"{reviewer_id}","summary":"one finding","findings":[{{"id":"f-1","impact_class":"correctness","proposed_severity":"info","category":"api","title":"Missing error handling","claim":"enqueue may panic on a full queue","explanation":"No bound check exists.","recommendation":"Return an error when full.","evidence":[{{"kind":"source","path":"src/queue.rs","excerpt":"fn enqueue"}}]}}],"candidate_project_facts":[],"model_metadata":{{}},"duration_ms":0}}"#
    )
}

fn script_json(replies: &[serde_json::Value]) -> String {
    serde_json::json!({
        "sessions": replies
            .iter()
            .map(|steps| serde_json::json!({
                "steps": steps,
            }))
            .collect::<Vec<_>>(),
    })
    .to_string()
}

const READY_TIMEOUT: Duration = Duration::from_secs(20);
const ROUND_TIMEOUT: Duration = Duration::from_secs(20);

fn no_tool_response(_request: &nisse::jury::protocol::ToolRequestPayload) -> ToolResponsePayload {
    // The scripted agent never invokes tools; failing loudly would hide a
    // pipeline bug, so answer with a bounded error instead.
    ToolResponsePayload::failure(
        "unused",
        "unexpected",
        "scripted reviewers do not call tools",
    )
}

/// Read the recorded scripted sessions (one JSON object per line).
fn read_records(path: &std::path::Path) -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(path).expect("record file readable");
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("record lines are JSON"))
        .collect()
}

#[test]
fn pi_runtime_maps_spec_to_isolated_session_and_completes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let script = dir.path().join("script.json");
    let record = dir.path().join("record.jsonl");
    std::fs::write(
        &script,
        script_json(&[serde_json::json!([{ "reply": valid_report("quality") }])]),
    )
    .expect("write script");

    let mut host = HostProcess::spawn(&launch(
        HostRuntime::Pi,
        &[
            ("NISSE_PI_SESSION_SCRIPT", script.to_str().unwrap()),
            ("NISSE_PI_SESSION_RECORD", record.to_str().unwrap()),
        ],
    ))
    .expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");
    host.send_host_config(&host_config(
        "acme/local-small",
        Some("http://127.0.0.1:8080/v1"),
    ))
    .expect("host_config failed");

    let payload = StartReviewPayload {
        review_id: "rev-map".to_string(),
        kind: ReviewKind::Plan,
        reviewers: vec![spec("quality", 10_000)],
    };
    host.start_review(&payload).expect("start_review failed");
    let outcomes = host
        .complete_review(
            "rev-map",
            &["quality".to_string()],
            &mut no_tool_response,
            ROUND_TIMEOUT,
        )
        .expect("review should complete");

    match &outcomes[0].1 {
        ReviewerOutcome::Completed(report) => {
            // Identity is stamped by the host from the spec, not the model.
            assert_eq!(report.reviewer_id, "quality");
            assert_eq!(report.findings.len(), 1);
            let metadata = report.model_metadata.as_object().expect("metadata object");
            assert_eq!(metadata.get("runtime"), Some(&serde_json::json!("pi")));
            assert_eq!(
                metadata.get("model"),
                Some(&serde_json::json!("acme/local-small"))
            );
            assert_eq!(
                metadata.get("endpoint"),
                Some(&serde_json::json!("http://127.0.0.1:8080/v1"))
            );
            assert_eq!(
                metadata.get("thinking_level"),
                Some(&serde_json::json!("medium"))
            );
            assert_eq!(metadata.get("turns"), Some(&serde_json::json!(1)));
            assert_eq!(metadata.get("repaired"), None);
        }
        other => panic!("expected completion, got {other:?}"),
    }

    let records = read_records(&record);
    assert_eq!(records.len(), 1, "exactly one scripted session expected");
    let config = &records[0]["config"];
    assert_eq!(config["reviewerId"], "quality");
    assert_eq!(config["model"], "acme/local-small");
    assert_eq!(config["endpoint"], "http://127.0.0.1:8080/v1");
    assert_eq!(config["thinkingLevel"], "medium");

    // The system prompt carries the reviewer methodology explicitly.
    let system_prompt = config["systemPrompt"].as_str().expect("system prompt");
    assert!(system_prompt.contains("Nisse reviewer `quality`"));
    assert!(system_prompt.contains("Work the checklist"));
    assert!(system_prompt.contains("1. Check error paths."));
    assert!(system_prompt.contains("2. Check shared state."));
    assert!(system_prompt.contains("exactly one JSON object"));

    // The first message carries the review task and the initial context.
    let prompts = records[0]["prompts"].as_array().expect("prompts");
    assert_eq!(prompts.len(), 1);
    let first = prompts[0].as_str().expect("prompt text");
    assert!(first.contains("plan review"));
    assert!(first.contains("src/queue.rs"));
    assert!(first.contains("fn enqueue(&mut self, item: T)"));

    // Tools: exactly the granted read-only capabilities.
    assert_eq!(
        config["toolNames"],
        serde_json::json!(["project_lookup", "source_lookup"])
    );

    // Isolation: no inherited Pi resources, in-memory only.
    let isolation = &config["isolation"];
    for flag in [
        "inMemorySession",
        "noBuiltinTools",
        "noExtensions",
        "noSkills",
        "noPromptTemplates",
        "noContextFiles",
        "noThemes",
    ] {
        assert_eq!(
            isolation[flag],
            serde_json::json!(true),
            "isolation flag {flag}"
        );
    }

    host.shutdown(Duration::from_secs(10)).expect("clean exit");
}

#[test]
fn pi_runtime_repairs_malformed_output_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let script = dir.path().join("script.json");
    std::fs::write(
        &script,
        script_json(&[serde_json::json!([
            { "reply": "Sorry, I cannot produce JSON right now." },
            { "reply": valid_report("quality") },
        ])]),
    )
    .expect("write script");

    let mut host = HostProcess::spawn(&launch(
        HostRuntime::Pi,
        &[("NISSE_PI_SESSION_SCRIPT", script.to_str().unwrap())],
    ))
    .expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");
    host.send_host_config(&host_config("acme/local-small", None))
        .expect("host_config failed");

    let payload = StartReviewPayload {
        review_id: "rev-repair".to_string(),
        kind: ReviewKind::Code,
        reviewers: vec![spec("quality", 10_000)],
    };
    host.start_review(&payload).expect("start_review failed");
    let outcomes = host
        .complete_review(
            "rev-repair",
            &["quality".to_string()],
            &mut no_tool_response,
            ROUND_TIMEOUT,
        )
        .expect("review should complete");

    match &outcomes[0].1 {
        ReviewerOutcome::Completed(report) => {
            let metadata = report.model_metadata.as_object().expect("metadata object");
            assert_eq!(metadata.get("turns"), Some(&serde_json::json!(2)));
            assert_eq!(metadata.get("repaired"), Some(&serde_json::json!(true)));
        }
        other => panic!("expected completion after repair, got {other:?}"),
    }
    host.shutdown(Duration::from_secs(10)).expect("clean exit");
}

#[test]
fn pi_runtime_fails_when_repair_stays_malformed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let script = dir.path().join("script.json");
    std::fs::write(
        &script,
        script_json(&[serde_json::json!([
            { "reply": "nope" },
            { "reply": "{\"still\":\"invalid\"}" },
        ])]),
    )
    .expect("write script");

    let mut host = HostProcess::spawn(&launch(
        HostRuntime::Pi,
        &[("NISSE_PI_SESSION_SCRIPT", script.to_str().unwrap())],
    ))
    .expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");
    host.send_host_config(&host_config("acme/local-small", None))
        .expect("host_config failed");

    let payload = StartReviewPayload {
        review_id: "rev-bad".to_string(),
        kind: ReviewKind::Plan,
        reviewers: vec![spec("quality", 10_000)],
    };
    host.start_review(&payload).expect("start_review failed");
    let outcomes = host
        .complete_review(
            "rev-bad",
            &["quality".to_string()],
            &mut no_tool_response,
            ROUND_TIMEOUT,
        )
        .expect("review should complete");

    match &outcomes[0].1 {
        ReviewerOutcome::Failed(info) => {
            assert_eq!(info.code, "invalid_report");
            assert!(
                info.message.contains("after repair"),
                "failure should describe both attempts: {}",
                info.message
            );
        }
        other => panic!("expected failure, got {other:?}"),
    }
    host.shutdown(Duration::from_secs(10)).expect("clean exit");
}

#[test]
fn pi_runtime_enforces_the_session_timeout() {
    let dir = tempfile::tempdir().expect("tempdir");
    let script = dir.path().join("script.json");
    std::fs::write(
        &script,
        script_json(&[serde_json::json!([{ "hangMs": 10_000 }])]),
    )
    .expect("write script");

    let mut host = HostProcess::spawn(&launch(
        HostRuntime::Pi,
        &[("NISSE_PI_SESSION_SCRIPT", script.to_str().unwrap())],
    ))
    .expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");
    host.send_host_config(&host_config("acme/local-small", None))
        .expect("host_config failed");

    let payload = StartReviewPayload {
        review_id: "rev-timeout".to_string(),
        kind: ReviewKind::Plan,
        reviewers: vec![spec("quality", 400)],
    };
    host.start_review(&payload).expect("start_review failed");
    let started = std::time::Instant::now();
    let outcomes = host
        .complete_review(
            "rev-timeout",
            &["quality".to_string()],
            &mut no_tool_response,
            ROUND_TIMEOUT,
        )
        .expect("review should complete with a failed reviewer");

    match &outcomes[0].1 {
        ReviewerOutcome::Failed(info) => {
            assert_eq!(info.code, "session_timeout");
        }
        other => panic!("expected timeout failure, got {other:?}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "session timeout must fire well before the round timeout"
    );
    host.shutdown(Duration::from_secs(10)).expect("clean exit");
}

#[test]
fn pi_runtime_without_model_profile_fails_closed() {
    let mut host = HostProcess::spawn(&launch(HostRuntime::Pi, &[])).expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");

    // No host_config at all: reviewer sessions must never guess a model.
    let payload = StartReviewPayload {
        review_id: "rev-nomodel".to_string(),
        kind: ReviewKind::Plan,
        reviewers: vec![spec("quality", 10_000)],
    };
    host.start_review(&payload).expect("start_review failed");
    let outcomes = host
        .complete_review(
            "rev-nomodel",
            &["quality".to_string()],
            &mut no_tool_response,
            ROUND_TIMEOUT,
        )
        .expect("review should complete with a failed reviewer");

    match &outcomes[0].1 {
        ReviewerOutcome::Failed(info) => {
            assert_eq!(info.code, "model_profile_unresolved");
            assert!(
                info.message.contains("standard"),
                "failure names the missing profile: {}",
                info.message
            );
        }
        other => panic!("expected model failure, got {other:?}"),
    }

    // Supplying host_config afterwards repairs later reviews.
    host.send_host_config(&host_config("acme/local-small", None))
        .expect("host_config failed");
    let dir = tempfile::tempdir().expect("tempdir");
    let script = dir.path().join("script.json");
    std::fs::write(
        &script,
        script_json(&[serde_json::json!([{ "reply": valid_report("quality") }])]),
    )
    .expect("write script");
    // The factory resolved without a script the first time; the script env
    // must be present at host start, so restart the host for this half.
    host.shutdown(Duration::from_secs(10)).expect("clean exit");

    let mut host = HostProcess::spawn(&launch(
        HostRuntime::Pi,
        &[("NISSE_PI_SESSION_SCRIPT", script.to_str().unwrap())],
    ))
    .expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");
    host.send_host_config(&host_config("acme/local-small", None))
        .expect("host_config failed");
    let payload = StartReviewPayload {
        review_id: "rev-model-ok".to_string(),
        kind: ReviewKind::Plan,
        reviewers: vec![spec("quality", 10_000)],
    };
    host.start_review(&payload).expect("start_review failed");
    let outcomes = host
        .complete_review(
            "rev-model-ok",
            &["quality".to_string()],
            &mut no_tool_response,
            ROUND_TIMEOUT,
        )
        .expect("review should complete");
    assert!(matches!(
        &outcomes[0].1,
        ReviewerOutcome::Completed(report) if report.reviewer_id == "quality"
    ));
    host.shutdown(Duration::from_secs(10)).expect("clean exit");
}

#[test]
fn retry_policy_retries_a_failed_reviewer_once_with_the_same_spec() {
    let dir = tempfile::tempdir().expect("tempdir");
    let attempts = dir.path().join("attempts.jsonl");
    let mut host = HostProcess::spawn(&launch(
        HostRuntime::Fake,
        &[
            ("NISSE_FAKE_REVIEWER_FAIL_ONCE_FOR", "security"),
            (
                "NISSE_FAKE_REVIEWER_ATTEMPTS_FILE",
                attempts.to_str().unwrap(),
            ),
        ],
    ))
    .expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");

    let quality = spec("quality", 10_000);
    let security = ReviewerExecutionSpec {
        reviewer_id: "security".to_string(),
        specialization: "security".to_string(),
        model_profile: "strong".to_string(),
        ..spec("security", 10_000)
    };
    let payload = StartReviewPayload {
        review_id: "rev-retry".to_string(),
        kind: ReviewKind::Code,
        reviewers: vec![quality, security],
    };
    let outcomes = execute_review(&mut host, &payload, &mut no_tool_response, ROUND_TIMEOUT)
        .expect("review should complete");

    // Both reviewers eventually complete: security failed once, then the
    // identical spec was retried and succeeded.
    assert_eq!(outcomes.len(), 2);
    for (reviewer_id, outcome) in &outcomes {
        match outcome {
            ReviewerOutcome::Completed(report) => {
                assert_eq!(report.reviewer_id, *reviewer_id);
            }
            other => panic!("expected completion for {reviewer_id}, got {other:?}"),
        }
    }

    let lines: Vec<serde_json::Value> = std::fs::read_to_string(attempts)
        .expect("attempts readable")
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON line"))
        .collect();
    let quality_attempts = lines
        .iter()
        .filter(|entry| entry["reviewer_id"] == "quality")
        .count();
    let security_attempts = lines
        .iter()
        .filter(|entry| entry["reviewer_id"] == "security")
        .count();
    assert_eq!(
        quality_attempts, 1,
        "quality must not be retried: {lines:?}"
    );
    assert_eq!(
        security_attempts, 2,
        "security must run exactly twice (initial + one retry): {lines:?}"
    );

    // The retry keeps the original reviewer order in the outcome list.
    assert_eq!(outcomes[0].0, "quality");
    assert!(
        outcomes[0]
            .1
            .as_report()
            .expect("quality completed")
            .summary
            .contains("attempt=1")
    );
    assert_eq!(outcomes[1].0, "security");
    assert!(
        outcomes[1]
            .1
            .as_report()
            .expect("security completed")
            .summary
            .contains("attempt=2")
    );

    host.shutdown(Duration::from_secs(10)).expect("clean exit");
}

#[test]
fn retry_policy_gives_up_after_exactly_one_retry() {
    let dir = tempfile::tempdir().expect("tempdir");
    let attempts = dir.path().join("attempts.jsonl");
    let mut host = HostProcess::spawn(&launch(
        HostRuntime::Fake,
        &[
            ("NISSE_FAKE_REVIEWER_FAIL_FOR", "security"),
            (
                "NISSE_FAKE_REVIEWER_ATTEMPTS_FILE",
                attempts.to_str().unwrap(),
            ),
        ],
    ))
    .expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");

    let payload = StartReviewPayload {
        review_id: "rev-fail".to_string(),
        kind: ReviewKind::Code,
        reviewers: vec![spec("quality", 10_000), spec("security", 10_000)],
    };
    let outcomes = execute_review(&mut host, &payload, &mut no_tool_response, ROUND_TIMEOUT)
        .expect("review should complete");

    assert!(matches!(&outcomes[0], (_, ReviewerOutcome::Completed(_))));
    match &outcomes[1] {
        (reviewer_id, ReviewerOutcome::Failed(info)) => {
            assert_eq!(reviewer_id, "security");
            assert_eq!(info.code, "session_error");
            assert!(
                info.message.contains("scripted fake failure"),
                "unexpected message: {}",
                info.message
            );
        }
        other => panic!("expected security failure, got {other:?}"),
    }

    let lines: Vec<serde_json::Value> = std::fs::read_to_string(attempts)
        .expect("attempts readable")
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON line"))
        .collect();
    let security_attempts = lines
        .iter()
        .filter(|entry| entry["reviewer_id"] == "security")
        .count();
    assert_eq!(
        security_attempts, 2,
        "exactly one retry is permitted: {lines:?}"
    );
    host.shutdown(Duration::from_secs(10)).expect("clean exit");
}
