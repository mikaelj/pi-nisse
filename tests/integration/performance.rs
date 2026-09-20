//! Performance measurements across the real Pi-host child process (plan
//! milestone 29): host startup latency and reviewer concurrency
//! obeying the configured bound.
//!
//! The library-level measurements (warm refresh, one-file refresh,
//! SQLite growth, record loading, staleness, observation grouping,
//! retrieval, routing, context bounds) live in `src/perf/mod.rs`.
//! Timing bounds here scale by `NISSE_PERF_SCALE` exactly like there.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use nisse::domain::context::ContextEntry;
use nisse::domain::model::ThinkingLevel;
use nisse::domain::review::ReviewKind;
use nisse::domain::reviewer::{ReviewerCapability, ReviewerExecutionSpec};
use nisse::jury::execution::execute_review;
use nisse::jury::host::{HostProcess, PiHostLaunch, ReviewerOutcome};
use nisse::jury::protocol::{
    HostConfigPayload, HostRuntime, StartReviewPayload, ToolResponsePayload,
};
use tempfile::TempDir;

const READY_TIMEOUT: Duration = Duration::from_secs(20);
const ROUND_TIMEOUT: Duration = Duration::from_secs(30);

/// Multiplier for every timing bound (see `src/perf/mod.rs`).
fn scale() -> f64 {
    std::env::var("NISSE_PERF_SCALE")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|factor| factor.is_finite() && *factor >= 1.0)
        .unwrap_or(1.0)
}

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

/// Spawn a host and time spawn → `host_ready`.
fn startup_latency(runtime: HostRuntime, env: &[(&str, &str)]) -> Duration {
    let mut host = HostProcess::spawn(&launch(runtime, env)).expect("spawn failed");
    let started = Instant::now();
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");
    let latency = started.elapsed();
    let _ = host.shutdown(Duration::from_secs(5));
    latency
}

#[test]
fn pi_host_startup_stays_interactive() {
    // The fake runtime measures the pure process boundary: Node start,
    // module loading, and the JSONL handshake. The pi runtime adds the
    // SDK import on first use; both must stay comfortably interactive.
    let fake = startup_latency(HostRuntime::Fake, &[]);
    eprintln!("perf: pi-host startup (fake runtime): {fake:?}");
    let pi = startup_latency(HostRuntime::Pi, &[]);
    eprintln!("perf: pi-host startup (pi runtime): {pi:?}");
    let bound = Duration::from_secs(5).mul_f64(scale());
    assert!(
        fake <= bound && pi <= bound,
        "pi-host startup {fake:?}/{pi:?} exceeded {bound:?}"
    );
}

fn spec(reviewer_id: &str) -> ReviewerExecutionSpec {
    ReviewerExecutionSpec {
        reviewer_id: reviewer_id.to_string(),
        specialization: "general correctness".to_string(),
        instructions: "Work the checklist; report only evidenced findings.".to_string(),
        checklist: vec!["Check error paths.".to_string()],
        model_profile: "standard".to_string(),
        thinking_level: ThinkingLevel::Medium,
        initial_context: vec![ContextEntry {
            kind: nisse::domain::context::ContextKind::Source,
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
        allowed_capabilities: vec![ReviewerCapability::SourceLookup],
        max_lookup_calls: 2,
        max_returned_context_bytes: 4096,
        timeout_ms: 20_000,
    }
}

fn valid_report(reviewer_id: &str) -> String {
    format!(
        r#"{{"reviewer_id":"{reviewer_id}","summary":"one finding","findings":[],"candidate_project_facts":[],"model_metadata":{{}},"duration_ms":0}}"#
    )
}

fn no_tool_response(_request: &nisse::jury::protocol::ToolRequestPayload) -> ToolResponsePayload {
    ToolResponsePayload::failure(
        "unused",
        "unexpected",
        "scripted reviewers do not call tools",
    )
}

/// Run one four-reviewer round in which every scripted session first
/// sleeps `hang` milliseconds, and return (elapsed, outcomes).
fn run_four_reviewers(max_concurrent: Option<u32>, hang_ms: u64) -> (Duration, Vec<String>) {
    let dir = TempDir::new().expect("tempdir");
    let script = dir.path().join("script.json");
    let sessions: Vec<serde_json::Value> = ["quality", "security", "testing", "performance"]
        .iter()
        .map(|reviewer| {
            serde_json::json!({
                "steps": [
                    { "hangMs": hang_ms },
                    { "reply": valid_report(reviewer) },
                ]
            })
        })
        .collect();
    std::fs::write(
        &script,
        serde_json::json!({ "sessions": sessions }).to_string(),
    )
    .expect("write script");

    let mut host = HostProcess::spawn(&launch(
        HostRuntime::Pi,
        &[(
            "NISSE_PI_SESSION_SCRIPT",
            script.to_str().expect("script path is UTF-8"),
        )],
    ))
    .expect("spawn failed");
    host.wait_ready(READY_TIMEOUT).expect("host_ready expected");
    let mut config = HostConfigPayload::from_config_profiles(
        &[("standard".to_string(), model_profile())]
            .into_iter()
            .collect(),
    );
    if let Some(max) = max_concurrent {
        config = config.with_max_concurrent_reviewers(max);
    }
    host.send_host_config(&config).expect("host_config failed");

    let payload = StartReviewPayload {
        review_id: "rev-concurrency".to_string(),
        kind: ReviewKind::Plan,
        reviewers: ["quality", "security", "testing", "performance"]
            .iter()
            .map(|reviewer| spec(reviewer))
            .collect(),
    };
    let started = Instant::now();
    let outcomes = execute_review(&mut host, &payload, &mut no_tool_response, ROUND_TIMEOUT)
        .expect("review round completed");
    let elapsed = started.elapsed();
    let _ = host.shutdown(Duration::from_secs(5));

    let mut completed = Vec::new();
    for (reviewer_id, outcome) in outcomes {
        match outcome {
            ReviewerOutcome::Completed(_) => completed.push(reviewer_id),
            ReviewerOutcome::Failed(error) => panic!(
                "reviewer `{reviewer_id}` failed: {} {}",
                error.code, error.message
            ),
        }
    }
    (elapsed, completed)
}

fn model_profile() -> nisse::domain::model::ModelProfile {
    nisse::domain::model::ModelProfile {
        model: "acme/local-small".to_string(),
        endpoint: Some("http://127.0.0.1:8080/v1".to_string()),
        thinking_level: Some(ThinkingLevel::Medium),
    }
}

#[test]
fn reviewer_concurrency_obeyes_the_configured_bound() {
    // Four scripted sessions, each sleeping 400 ms before replying.
    //
    // - bounded at 2: two waves of two → at least ~800 ms;
    // - default bound 4: one wave → close to ~400 ms.
    //
    // The wide margins make the wall-clock differential robust while
    // still proving the configured bound changes scheduling (and that
    // the bound does not serialize reviewers one by one).
    let hang = 400u64;
    let (limited, completed_limited) = run_four_reviewers(Some(2), hang);
    eprintln!("perf: four hanging reviewers, bound 2: {limited:?}");
    let (default, completed_default) = run_four_reviewers(None, hang);
    eprintln!("perf: four hanging reviewers, default bound: {default:?}");

    assert_eq!(completed_limited.len(), 4, "every reviewer completes");
    assert_eq!(completed_default.len(), 4, "every reviewer completes");

    let scale = scale();
    let wave = Duration::from_millis(hang);
    // Two waves cannot finish faster than (roughly) twice one hang.
    assert!(
        limited >= wave.mul_f64(2.0).saturating_sub(Duration::from_millis(100)),
        "bound 2 did not schedule two waves: {limited:?}"
    );
    // The default bound runs one wave: comfortably below 1.5 hangs
    // (serialization or a mistaken bound of 1 would take at least two).
    let one_wave_bound = wave.mul_f64(1.5).mul_f64(scale);
    assert!(
        default <= one_wave_bound,
        "default bound {default:?} exceeds one wave {one_wave_bound:?}; \
         reviewers appear serialized"
    );
}
