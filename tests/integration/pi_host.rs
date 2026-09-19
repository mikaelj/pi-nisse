//! Integration tests for the Pi review host process boundary
//! (plan milestone 11: Rust/TypeScript protocol before real reviewer
//! execution).
//!
//! These tests spawn the real TypeScript host (`runtime/pi-host`) with the
//! deterministic fake reviewer runtime and exercise the JSONL protocol:
//! startup, protocol version, request correlation, malformed JSON,
//! timeouts, child crashes, stderr isolation, and stdout purity. No
//! network access and no LLM provider are required.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use nisse::domain::context::{ContextBundle, ContextEntry, ContextKind};
use nisse::domain::model::ThinkingLevel;
use nisse::domain::review::ReviewKind;
use nisse::domain::reviewer::{ReviewerCapability, ReviewerExecutionSpec};
use nisse::jury::host::{HostError, HostProcess, PiHostLaunch};
use nisse::jury::protocol::{HostEvent, HostRuntime, StartReviewPayload, ToolResponsePayload};

fn host_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("runtime/pi-host/src/main.ts")
}

fn fake_launch(fault: Option<&str>, extra: &[(&str, &str)]) -> PiHostLaunch {
    let mut launch = PiHostLaunch::new("node", host_script(), HostRuntime::Fake);
    if let Some(fault) = fault {
        launch = launch.env("NISSE_PI_HOST_FAULT", fault);
    }
    for (key, value) in extra {
        launch = launch.env(key, value);
    }
    launch
}

fn spec(reviewer_id: &str, grant_lookup: bool) -> ReviewerExecutionSpec {
    ReviewerExecutionSpec {
        reviewer_id: reviewer_id.to_string(),
        specialization: format!("{reviewer_id} specialization"),
        instructions: "Work the checklist.".to_string(),
        checklist: vec!["Check error paths.".to_string()],
        model_profile: "standard".to_string(),
        thinking_level: ThinkingLevel::Medium,
        initial_context: vec![ContextEntry {
            kind: ContextKind::SymbolDefinition,
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
        allowed_capabilities: if grant_lookup {
            vec![
                ReviewerCapability::ProjectLookup,
                ReviewerCapability::SourceLookup,
            ]
        } else {
            vec![ReviewerCapability::SourceLookup]
        },
        max_lookup_calls: if grant_lookup { 4 } else { 0 },
        max_returned_context_bytes: 65_536,
        timeout_ms: 30_000,
    }
}

fn review_payload(review_id: &str, reviewers: Vec<ReviewerExecutionSpec>) -> StartReviewPayload {
    StartReviewPayload {
        review_id: review_id.to_string(),
        kind: ReviewKind::Plan,
        reviewers,
    }
}

fn bundle_with_entries(count: usize) -> ContextBundle {
    ContextBundle {
        entries: (0..count)
            .map(|i| ContextEntry {
                kind: ContextKind::SourceSkeleton,
                title: format!("skeleton {i}"),
                path: Some(format!("src/file{i}.rs")),
                symbol: None,
                line_start: None,
                line_end: None,
                record_id: None,
                practice_id: None,
                text: format!("skeleton text {i}"),
                score: 0.5,
            })
            .collect(),
        truncated: false,
        text_bytes: 42 * count,
    }
}

const READY_TIMEOUT: Duration = Duration::from_secs(20);
const REVIEW_TIMEOUT: Duration = Duration::from_secs(20);

#[test]
fn startup_handshake_announces_the_fake_runtime() {
    let mut host = HostProcess::spawn(&fake_launch(None, &[])).expect("spawn failed");
    let ready = host.wait_ready(READY_TIMEOUT).expect("host_ready expected");
    assert_eq!(ready.runtime, HostRuntime::Fake);
    host.shutdown(Duration::from_secs(10)).expect("clean exit");
}

#[test]
fn wrong_protocol_version_from_host_is_a_typed_error() {
    let mut host =
        HostProcess::spawn(&fake_launch(Some("wrong-version"), &[])).expect("spawn failed");
    let err = host.wait_ready(READY_TIMEOUT).expect_err("must fail");
    assert!(
        matches!(
            &err,
            HostError::Protocol(nisse::jury::protocol::ProtocolError::UnsupportedVersion {
                got: 99,
                expected: 1
            })
        ),
        "expected an unsupported-version error, got {err:?}"
    );
}

#[test]
fn wrong_protocol_version_toward_host_reports_host_error() {
    let mut host = HostProcess::spawn(&fake_launch(None, &[])).expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");
    host.send_raw_line(r#"{"protocol_version":2,"id":"x-1","type":"cancel"}"#)
        .expect("write failed");
    match host.recv_event(READY_TIMEOUT).expect("host_error expected") {
        HostEvent::HostError(info) => {
            assert_eq!(info.code, "unsupported_protocol_version");
        }
        other => panic!("expected host_error, got {other:?}"),
    }
    host.shutdown(Duration::from_secs(10)).expect("clean exit");
}

#[test]
fn tool_request_and_response_are_correlated() {
    let mut host = HostProcess::spawn(&fake_launch(None, &[])).expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");

    let payload = review_payload("rev-correlate", vec![spec("quality", true)]);
    host.start_review(&payload).expect("start_review failed");

    let mut answered = Vec::new();
    let outcomes = host
        .complete_review(
            "rev-correlate",
            &["quality".to_string()],
            &mut |request| {
                answered.push((request.tool, request.arguments.clone()));
                ToolResponsePayload::success("ignored", bundle_with_entries(2))
            },
            REVIEW_TIMEOUT,
        )
        .expect("review should complete");

    assert_eq!(answered.len(), 1, "exactly one tool request expected");
    assert_eq!(answered[0].0, ReviewerCapability::ProjectLookup);
    assert_eq!(
        answered[0].1["query"],
        serde_json::json!("nisse fake runtime probe")
    );

    assert_eq!(outcomes.len(), 1);
    match &outcomes[0].1 {
        nisse::jury::host::ReviewerOutcome::Completed(report) => {
            assert_eq!(report.reviewer_id, "quality");
            assert!(
                report.summary.contains("tools=1"),
                "summary must reflect tool use: {}",
                report.summary
            );
            assert!(
                report.summary.contains("tool_entries=2"),
                "summary must reflect the answered bundle: {}",
                report.summary
            );
            assert_eq!(report.findings.len(), 1);
            assert_eq!(report.findings[0].id, "fake-1");
        }
        other => panic!("expected completion, got {other:?}"),
    }
    host.shutdown(Duration::from_secs(10)).expect("clean exit");
}

#[test]
fn tool_error_responses_are_delivered_to_the_session() {
    let mut host = HostProcess::spawn(&fake_launch(None, &[])).expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");

    let payload = review_payload("rev-tool-error", vec![spec("quality", true)]);
    host.start_review(&payload).expect("start_review failed");
    let outcomes = host
        .complete_review(
            "rev-tool-error",
            &["quality".to_string()],
            &mut |_| ToolResponsePayload::failure("ignored", "denied", "capability not granted"),
            REVIEW_TIMEOUT,
        )
        .expect("review should complete");

    match &outcomes[0].1 {
        nisse::jury::host::ReviewerOutcome::Completed(report) => {
            assert!(
                report
                    .summary
                    .contains("tool_error=denied: capability not granted"),
                "summary must reflect the tool failure: {}",
                report.summary
            );
        }
        other => panic!("expected completion, got {other:?}"),
    }
    host.shutdown(Duration::from_secs(10)).expect("clean exit");
}

#[test]
fn reviewers_without_lookup_grants_never_call_tools() {
    let mut host = HostProcess::spawn(&fake_launch(None, &[])).expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");

    let payload = review_payload("rev-no-tools", vec![spec("quality", false)]);
    host.start_review(&payload).expect("start_review failed");
    let outcomes = host
        .complete_review(
            "rev-no-tools",
            &["quality".to_string()],
            &mut |_| panic!("no tool request expected"),
            REVIEW_TIMEOUT,
        )
        .expect("review should complete");

    match &outcomes[0].1 {
        nisse::jury::host::ReviewerOutcome::Completed(report) => {
            assert!(
                report.summary.contains("tools=0"),
                "summary must reflect zero tool use: {}",
                report.summary
            );
        }
        other => panic!("expected completion, got {other:?}"),
    }
    host.shutdown(Duration::from_secs(10)).expect("clean exit");
}

#[test]
fn failed_reviewers_report_reviewer_failed_while_others_complete() {
    let mut host = HostProcess::spawn(&fake_launch(
        None,
        &[("NISSE_FAKE_REVIEWER_FAIL_FOR", "security")],
    ))
    .expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");

    let payload = review_payload(
        "rev-partial",
        vec![spec("quality", false), spec("security", false)],
    );
    host.start_review(&payload).expect("start_review failed");
    let outcomes = host
        .complete_review(
            "rev-partial",
            &["quality".to_string(), "security".to_string()],
            &mut |_| panic!("no tool request expected"),
            REVIEW_TIMEOUT,
        )
        .expect("review should complete");

    let mut completed = 0;
    let mut failed = 0;
    for (reviewer_id, outcome) in &outcomes {
        match outcome {
            nisse::jury::host::ReviewerOutcome::Completed(report) => {
                completed += 1;
                assert_eq!(report.reviewer_id, *reviewer_id);
            }
            nisse::jury::host::ReviewerOutcome::Failed(info) => {
                failed += 1;
                assert_eq!(reviewer_id, "security");
                assert_eq!(info.code, "session_error");
                assert!(
                    info.message.contains("scripted fake failure"),
                    "unexpected failure message: {}",
                    info.message
                );
            }
        }
    }
    assert_eq!(completed, 1, "quality must complete: {outcomes:?}");
    assert_eq!(failed, 1, "security must fail: {outcomes:?}");
    host.shutdown(Duration::from_secs(10)).expect("clean exit");
}

#[test]
fn malformed_stdout_line_from_host_is_a_typed_error() {
    let mut host =
        HostProcess::spawn(&fake_launch(Some("malformed-after-ready"), &[])).expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");

    let payload = review_payload("rev-malformed", vec![spec("quality", false)]);
    host.start_review(&payload).expect("start_review failed");
    let err = host
        .complete_review(
            "rev-malformed",
            &["quality".to_string()],
            &mut |_| panic!("no tool request expected"),
            REVIEW_TIMEOUT,
        )
        .expect_err("must fail");
    // A non-JSON stdout line is rejected as a malformed line; the reader
    // thread reports encoding/size violations, the envelope parser reports
    // JSON syntax violations. Both are deterministic typed failures.
    let malformed = matches!(
        &err,
        HostError::MalformedHostLine { .. }
            | HostError::Protocol(nisse::jury::protocol::ProtocolError::MalformedLine { .. })
    );
    assert!(malformed, "expected a malformed-line error, got {err:?}");
}

#[test]
fn malformed_input_toward_host_reports_host_error() {
    let mut host = HostProcess::spawn(&fake_launch(None, &[])).expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");

    host.send_raw_line("{not json").expect("write failed");
    match host.recv_event(READY_TIMEOUT).expect("host_error expected") {
        HostEvent::HostError(info) => assert_eq!(info.code, "malformed_message"),
        other => panic!("expected host_error, got {other:?}"),
    }

    host.send_raw_line("").expect("write failed");
    match host.recv_event(READY_TIMEOUT).expect("host_error expected") {
        HostEvent::HostError(info) => assert_eq!(info.code, "malformed_message"),
        other => panic!("expected host_error, got {other:?}"),
    }
    host.shutdown(Duration::from_secs(10)).expect("clean exit");
}

#[test]
fn unsupported_vocabulary_messages_are_refused_deterministically() {
    let mut host = HostProcess::spawn(&fake_launch(None, &[])).expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");

    // `start_fact_verification` is frozen vocabulary but belongs to a
    // later milestone.
    host.send_raw_line(r#"{"protocol_version":1,"id":"x-2","type":"start_fact_verification"}"#)
        .expect("write failed");
    match host.recv_event(READY_TIMEOUT).expect("host_error expected") {
        HostEvent::HostError(info) => {
            assert_eq!(info.code, "unsupported_message");
            assert!(info.message.contains("start_fact_verification"));
        }
        other => panic!("expected host_error, got {other:?}"),
    }
    host.shutdown(Duration::from_secs(10)).expect("clean exit");
}

#[test]
fn startup_timeout_is_a_typed_error() {
    let mut host =
        HostProcess::spawn(&fake_launch(Some("startup-hang"), &[])).expect("spawn failed");
    let err = host
        .wait_ready(Duration::from_millis(750))
        .expect_err("must time out");
    assert!(
        matches!(err, HostError::Timeout { .. }),
        "expected Timeout, got {err:?}"
    );
}

#[test]
fn review_timeout_is_a_typed_error() {
    let mut host =
        HostProcess::spawn(&fake_launch(Some("review-hang"), &[])).expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");

    let payload = review_payload("rev-hang", vec![spec("quality", false)]);
    host.start_review(&payload).expect("start_review failed");
    let err = host
        .complete_review(
            "rev-hang",
            &["quality".to_string()],
            &mut |_| panic!("no tool request expected"),
            Duration::from_millis(750),
        )
        .expect_err("must time out");
    assert!(
        matches!(err, HostError::Timeout { .. }),
        "expected Timeout, got {err:?}"
    );
}

#[test]
fn child_crash_before_ready_is_a_typed_error() {
    let mut host =
        HostProcess::spawn(&fake_launch(Some("crash-on-start"), &[])).expect("spawn failed");
    let err = host.wait_ready(READY_TIMEOUT).expect_err("must crash");
    match &err {
        HostError::ChildTerminated { status, .. } => {
            assert!(status.contains("exit code 3"), "status was {status}");
        }
        other => panic!("expected ChildTerminated, got {other:?}"),
    }
}

#[test]
fn child_exit_after_ready_is_a_typed_error() {
    let mut host =
        HostProcess::spawn(&fake_launch(Some("ready-then-exit"), &[])).expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");

    let payload = review_payload("rev-exit", vec![spec("quality", false)]);
    let err = host
        .start_review(&payload)
        .err()
        .or_else(|| {
            host.complete_review(
                "rev-exit",
                &["quality".to_string()],
                &mut |_| panic!("no tool request expected"),
                REVIEW_TIMEOUT,
            )
            .err()
        })
        .expect("must fail after child exit");
    match &err {
        HostError::ChildTerminated { status, .. } => {
            assert!(status.contains("exit code 0"), "status was {status}");
        }
        other => panic!("expected ChildTerminated, got {other:?}"),
    }
}

#[test]
fn cancel_stops_pending_work_and_shutdown_is_clean() {
    let mut host =
        HostProcess::spawn(&fake_launch(Some("review-hang"), &[])).expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");

    let payload = review_payload("rev-cancel", vec![spec("quality", false)]);
    host.start_review(&payload).expect("start_review failed");
    host.send_cancel(None).expect("cancel failed");

    let status = host.shutdown(Duration::from_secs(10)).expect("clean exit");
    assert!(
        status.success(),
        "host should exit 0 after cancel + stdin close"
    );
}

#[test]
fn stderr_stays_off_the_protocol_channel() {
    let mut host =
        HostProcess::spawn(&fake_launch(Some("stderr-noise"), &[])).expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");

    let payload = review_payload("rev-stderr", vec![spec("quality", false)]);
    host.start_review(&payload).expect("start_review failed");
    let outcomes = host
        .complete_review(
            "rev-stderr",
            &["quality".to_string()],
            &mut |_| panic!("no tool request expected"),
            REVIEW_TIMEOUT,
        )
        .expect("review should complete");
    assert_eq!(outcomes.len(), 1);

    let stderr = host.stderr_text();
    assert!(
        stderr.contains("nisse-pi-host: fault stderr-noise"),
        "expected scripted stderr diagnostics, got: {stderr}"
    );
    // The stderr marker must never leak into any protocol event; every
    // stdout line was strict-parsed by the client during the exchange.
    host.shutdown(Duration::from_secs(10)).expect("clean exit");
}

/// Spawn the host directly (without the typed client) and assert that
/// every stdout line is a well-formed protocol envelope: stdout carries
/// protocol messages only.
#[test]
fn stdout_carries_protocol_messages_only() {
    use std::io::{BufRead, BufReader};

    let mut child = Command::new("node")
        .arg(host_script())
        .arg("--runtime")
        .arg("fake")
        .env("NISSE_PI_HOST_FAULT", "stderr-noise")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn host");

    // Reader thread with a deadline so a hung host fails the test instead
    // of blocking it forever.
    let stdout = child.stdout.take().expect("stdout piped");
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    if tx.send(line).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });

    let mut saw_ready = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        let line = rx
            .recv_timeout(Duration::from_millis(200))
            .ok()
            .unwrap_or_default();
        if line.is_empty() {
            continue;
        }
        // Every line must be a strict protocol envelope.
        let envelope = nisse::jury::protocol::Envelope::parse_host_line(&line)
            .unwrap_or_else(|err| panic!("non-protocol stdout line {line:?}: {err:?}"));
        if envelope.message_type == "host_ready" {
            saw_ready = true;
            break;
        }
    }
    assert!(saw_ready, "host_ready must arrive on stdout");

    // Close stdin; the host must exit cleanly without writing anything
    // further to stdout.
    drop(child.stdin.take());
    let status = child.wait().expect("host exits");
    assert!(status.success());
    assert!(
        rx.recv_timeout(Duration::from_millis(500)).is_err(),
        "no further stdout traffic may follow stdin close"
    );
}
