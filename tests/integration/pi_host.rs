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
use nisse::domain::facts::FactVerificationVerdict;
use nisse::domain::model::ThinkingLevel;
use nisse::domain::review::ReviewKind;
use nisse::domain::reviewer::{ReviewerCapability, ReviewerExecutionSpec};
use nisse::jury::host::{HostError, HostProcess, PiHostLaunch};
use nisse::jury::protocol::{
    FactVerificationPayload, HostConfigPayload, HostEvent, HostRuntime, ModelCapabilitiesPayload,
    StartReviewPayload, ToolResponsePayload,
};

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
                got: 100,
                expected: 2
            })
        ),
        "expected an unsupported-version error, got {err:?}"
    );
}

#[test]
fn wrong_protocol_version_toward_host_reports_host_error() {
    let mut host = HostProcess::spawn(&fake_launch(None, &[])).expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");
    host.send_raw_line(r#"{"protocol_version":3,"id":"x-1","type":"cancel"}"#)
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
fn oversized_stdout_line_from_host_is_a_typed_error() {
    // Milestone 28: a host (or injected content relayed by a host) that
    // exceeds the frozen line bound is rejected while reading, without
    // buffering the oversized line.
    let mut host =
        HostProcess::spawn(&fake_launch(Some("oversized-line"), &[])).expect("spawn failed");
    let err = host.wait_ready(READY_TIMEOUT).expect_err("must fail");
    match &err {
        HostError::MalformedHostLine { reason } => {
            assert!(reason.contains("byte protocol bound"), "{reason}");
        }
        other => panic!("expected a malformed-line error, got {other:?}"),
    }
}

#[test]
fn non_utf8_stdout_from_host_is_a_typed_error() {
    // Milestone 28: stdout is a strict UTF-8 JSONL channel; binary noise
    // fails closed.
    let mut host =
        HostProcess::spawn(&fake_launch(Some("binary-stdout"), &[])).expect("spawn failed");
    let err = host.wait_ready(READY_TIMEOUT).expect_err("must fail");
    match &err {
        HostError::MalformedHostLine { reason } => {
            assert!(reason.contains("UTF-8"), "{reason}");
        }
        other => panic!("expected a malformed-line error, got {other:?}"),
    }
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

    // Every frozen vocabulary entry is active in this host version
    // (milestone 22 activated `start_fact_synthesis`), so an
    // active-type message with an invalid payload is refused as
    // `invalid_payload` — deterministically, never guessed.
    host.send_raw_line(r#"{"protocol_version":2,"id":"x-2","type":"start_fact_synthesis"}"#)
        .expect("write failed");
    match host.recv_event(READY_TIMEOUT).expect("host_error expected") {
        HostEvent::HostError(info) => {
            assert_eq!(info.code, "invalid_payload");
            // The strict payload validator names the offending field.
            assert!(info.message.contains("facts"), "{}", info.message);
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

// Linger fixtures: a Node child cannot close its stdio fds while alive
// (libuv refuses to close fds 0-2), so the end-to-end linger scenarios
// use small POSIX shell fixtures that close the real descriptor and keep
// running. `sh` is unavailable on Windows, where the cross-platform
// `reap_child` unit test in src/jury/host.rs covers the bounded
// kill-and-reap mechanics instead.
#[cfg(unix)]
fn linger_launch(fixture: &str) -> PiHostLaunch {
    PiHostLaunch::new(
        "sh",
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/host")
            .join(fixture),
        HostRuntime::Fake,
    )
}

#[cfg(unix)]
#[test]
fn dropping_a_living_child_kills_and_reaps_within_the_budget() {
    // `linger-silent.sh` never writes anything and lives ~15 s. No error
    // path runs first, so Drop's kill_and_reap (and the bounded joins)
    // must terminate on their own within the budget.
    let host = HostProcess::spawn(&linger_launch("linger-silent.sh")).expect("spawn failed");
    let started = std::time::Instant::now();
    drop(host);
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "bounded drop took {elapsed:?}; the fixture lingers 15 s with open pipes"
    );
}

#[cfg(unix)]
#[test]
fn stdout_eof_with_lingering_child_returns_within_the_reap_budget() {
    // `linger-eof.sh` closes stdout, then lives on for ~15 s. The old
    // unbounded `wait()` blocked here for the child's whole lifetime;
    // the bounded reaper must return a typed error (after killing the
    // child) well inside that window. The bound is generous so slow CI
    // runners cannot flake; the fixture lingers for 15 s.
    let mut host = HostProcess::spawn(&linger_launch("linger-eof.sh")).expect("spawn failed");
    let started = std::time::Instant::now();
    let err = host.wait_ready(READY_TIMEOUT).expect_err("must terminate");
    let elapsed = started.elapsed();
    assert!(
        matches!(err, HostError::ChildTerminated { .. }),
        "expected ChildTerminated, got {err:?}"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "bounded reap took {elapsed:?} (err {err:?}); the fixture lingers 15 s, so this must be the reaper, not the child exiting"
    );
}

#[cfg(unix)]
#[test]
fn broken_stdin_pipe_with_lingering_child_returns_within_the_reap_budget() {
    // `linger-pipe.sh` closes stdin BEFORE announcing host_ready, so the
    // core's first stdin write after reading the announcement hits a
    // closed pipe deterministically while the child still lives.
    let mut host = HostProcess::spawn(&linger_launch("linger-pipe.sh")).expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");

    let payload = HostConfigPayload::from_config_profiles(&Default::default());
    let started = std::time::Instant::now();
    let err = host
        .send_host_config(&payload)
        .expect_err("write must fail on the closed pipe");
    let elapsed = started.elapsed();
    assert!(
        matches!(
            err,
            HostError::ChildTerminated { .. } | HostError::WriteFailed(_)
        ),
        "expected ChildTerminated or WriteFailed, got {err:?}"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "bounded reap took {elapsed:?}; the fixture lingers 15 s, so this must be the reaper, not the child exiting"
    );
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

// ---------------------------------------------------------------------------
// Independent fact verification (plan section 47)
// ---------------------------------------------------------------------------

fn verification_payload(verification_id: &str, claim: &str) -> FactVerificationPayload {
    use nisse::domain::evidence::{EvidenceKind, EvidenceRef};
    FactVerificationPayload {
        verification_id: verification_id.to_string(),
        timeout_ms: 5_000,
        model_profile: "standard".to_string(),
        thinking_level: ThinkingLevel::Medium,
        claim: claim.to_string(),
        evidence: vec![EvidenceRef {
            kind: EvidenceKind::Source,
            path: Some("src/queue.rs".to_string()),
            symbol: None,
            line_start: Some(1),
            line_end: Some(3),
            content_key: None,
            plan_section: None,
            record_id: None,
            input_part: None,
            excerpt: "pub struct Queue { slots: [u8; 32] }".to_string(),
        }],
    }
}

#[test]
fn fact_verification_roundtrips_through_the_host() {
    let mut host = HostProcess::spawn(&fake_launch(None, &[])).expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");

    // Default fake verdict: insufficient (the fail-safe verdict).
    let payload = verification_payload("ver-default", "The queue is bounded.");
    host.start_fact_verification(&payload).expect("send failed");
    let result = host
        .complete_fact_verification("ver-default", REVIEW_TIMEOUT)
        .expect("completion failed");
    assert_eq!(result.verification_id, "ver-default");
    assert_eq!(result.verdict, FactVerificationVerdict::Insufficient);
    assert!(result.failure.is_none());
    host.shutdown(Duration::from_secs(10)).expect("clean exit");
}

#[test]
fn fact_verification_honors_scripted_fake_verdicts_and_faults() {
    let mut host = HostProcess::spawn(&fake_launch(
        None,
        &[(
            "NISSE_FAKE_FACT_VERIFICATION_VERDICTS",
            "supported,contradicted",
        )],
    ))
    .expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");

    for expected in [
        FactVerificationVerdict::Supported,
        FactVerificationVerdict::Contradicted,
        // Exhausted script falls back to insufficient.
        FactVerificationVerdict::Insufficient,
    ] {
        let id = format!("ver-{expected:?}");
        let payload = verification_payload(&id, "The queue is bounded.");
        host.start_fact_verification(&payload).expect("send failed");
        let result = host
            .complete_fact_verification(&id, REVIEW_TIMEOUT)
            .expect("completion failed");
        assert_eq!(result.verdict, expected);
        assert!(result.failure.is_none());
    }
    host.shutdown(Duration::from_secs(10)).expect("clean exit");

    // The scripted failure forces insufficient plus an error.
    let mut host = HostProcess::spawn(&fake_launch(
        None,
        &[("NISSE_FAKE_FACT_VERIFICATION_FAIL", "1")],
    ))
    .expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");
    let payload = verification_payload("ver-fail", "The queue is bounded.");
    host.start_fact_verification(&payload).expect("send failed");
    let result = host
        .complete_fact_verification("ver-fail", REVIEW_TIMEOUT)
        .expect("completion failed");
    assert_eq!(result.verdict, FactVerificationVerdict::Insufficient);
    assert!(
        result
            .failure
            .as_deref()
            .is_some_and(|failure| failure.contains("scripted_fake_failure")),
        "failure: {:?}",
        result.failure
    );
    host.shutdown(Duration::from_secs(10)).expect("clean exit");
}

#[test]
fn fact_verification_mismatched_ids_are_typed_errors() {
    let mut host = HostProcess::spawn(&fake_launch(None, &[])).expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");

    let payload = verification_payload("ver-real", "The queue is bounded.");
    host.start_fact_verification(&payload).expect("send failed");
    let err = host
        .complete_fact_verification("ver-other", REVIEW_TIMEOUT)
        .expect_err("mismatched id must fail");
    assert!(
        matches!(err, HostError::UnexpectedHostMessage { ref reason } if reason.contains("ver-real")),
        "{err:?}"
    );
    host.shutdown(Duration::from_secs(10)).expect("clean exit");
}

#[test]
fn fact_verification_rejects_malformed_payloads() {
    let mut host = HostProcess::spawn(&fake_launch(None, &[])).expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");

    // Missing evidence: the payload never reaches a session.
    host.send_raw_line(
        r#"{"protocol_version":2,"id":"x-9","type":"start_fact_verification","verification_id":"v","timeout_ms":5000,"model_profile":"standard","thinking_level":"medium","claim":"c","evidence":[]}"#,
    )
    .expect("write failed");
    match host.recv_event(READY_TIMEOUT).expect("host_error expected") {
        HostEvent::HostError(info) => {
            assert_eq!(info.code, "invalid_payload");
            assert!(info.message.contains("evidence"), "{info:?}");
        }
        other => panic!("expected host_error, got {other:?}"),
    }
    host.shutdown(Duration::from_secs(10)).expect("clean exit");
}

// ---- model_capabilities (protocol version 2) -------------------------

fn config_payload() -> HostConfigPayload {
    HostConfigPayload::from_config_profiles(
        &[(
            "standard".to_string(),
            nisse::domain::model::ModelProfile {
                model: "zai/glm-5.3".to_string(),
                endpoint: None,
                thinking_level: None,
            },
        )]
        .into_iter()
        .collect(),
    )
}

#[test]
fn host_config_is_answered_by_model_capabilities_echoing_the_config() {
    let mut host = HostProcess::spawn(&fake_launch(None, &[])).expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");
    host.send_host_config(&config_payload())
        .expect("host_config failed");
    let caps: ModelCapabilitiesPayload = host
        .wait_model_capabilities(READY_TIMEOUT)
        .expect("model_capabilities expected");
    let entry = caps.profiles.get("standard").expect("standard resolved");
    assert_eq!(entry.model, "zai/glm-5.3");
    // The fake runtime is the permissive double: all seven levels.
    assert_eq!(
        entry.supported_thinking_levels,
        nisse::domain::model::ThinkingLevel::PI_ORDER
            .into_iter()
            .collect(),
    );
    assert!(caps.unresolved.is_empty());
    host.shutdown(Duration::from_secs(10)).expect("clean exit");
}

#[test]
fn a_re_sent_host_config_yields_a_fresh_capabilities_reply() {
    let mut host = HostProcess::spawn(&fake_launch(None, &[])).expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");

    host.send_host_config(&config_payload())
        .expect("first host_config failed");
    let first = host
        .wait_model_capabilities(READY_TIMEOUT)
        .expect("first capabilities expected");

    host.send_host_config(&config_payload())
        .expect("second host_config failed");
    let second = host
        .wait_model_capabilities(READY_TIMEOUT)
        .expect("second capabilities expected");
    assert_ne!(
        first.host_config_id, second.host_config_id,
        "each host_config is answered by its own correlated reply"
    );
    host.shutdown(Duration::from_secs(10)).expect("clean exit");
}

#[test]
fn capabilities_env_override_restricts_reported_levels() {
    let mut host = HostProcess::spawn(&fake_launch(
        None,
        &[(
            "NISSE_PI_HOST_CAPABILITIES",
            r#"{"standard":["low","max"]}"#,
        )],
    ))
    .expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");
    host.send_host_config(&config_payload())
        .expect("host_config failed");
    let caps = host
        .wait_model_capabilities(READY_TIMEOUT)
        .expect("model_capabilities expected");
    let entry = caps.profiles.get("standard").expect("standard resolved");
    let levels: Vec<String> = entry
        .supported_thinking_levels
        .iter()
        .map(|level| level.to_string())
        .collect();
    assert_eq!(levels, ["low", "max"]);
    host.shutdown(Duration::from_secs(10)).expect("clean exit");
}

#[test]
fn a_host_that_never_reports_capabilities_fails_typed_on_timeout() {
    // The capabilities fixture announces host_ready and then goes
    // silent without answering host_config.
    let mut host =
        HostProcess::spawn(&linger_launch("capabilities-silent.sh")).expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");
    // send_host_config writes succeed (stdin still open); the reply
    // never comes, so a short explicit budget must fail typed.
    host.send_host_config(&config_payload())
        .expect("host_config write failed");
    let err = host
        .wait_model_capabilities(Duration::from_secs(2))
        .expect_err("must time out");
    assert!(
        matches!(err, HostError::Timeout { ref what } if what.contains("model capabilities")),
        "expected a typed capabilities timeout, got {err:?}"
    );
    let _ = host.shutdown(Duration::from_secs(2));
}

#[test]
fn wait_model_capabilities_without_a_sent_config_is_a_typed_error() {
    let mut host = HostProcess::spawn(&fake_launch(None, &[])).expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");
    let err = host
        .wait_model_capabilities(Duration::from_secs(2))
        .expect_err("must fail without host_config");
    assert!(
        matches!(err, HostError::UnexpectedHostMessage { .. }),
        "expected a typed ordering error, got {err:?}"
    );
    host.shutdown(Duration::from_secs(10)).expect("clean exit");
}
