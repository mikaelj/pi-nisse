//! Binary-level integration tests for `nisse eval routing` and
//! `nisse eval retrieval` (plan section 69, milestone 23).
//!
//! Both commands run the frozen fixtures fully offline: evaluation
//! builds its own temporary fixture repositories, so the commands work
//! from any directory. The frozen CLI has no `--json` flag for eval;
//! the machine-readable report is always emitted after the
//! `machine-readable:` marker. A canned loopback Needle server plays
//! the optional provider so the rules/Needle comparison variants are
//! exercised end to end without any network access beyond loopback.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;

use super::{run_nisse_in_with_env, write_user_config};
use tempfile::TempDir;

type Handler = Arc<dyn Fn(&str, &str) -> String + Send + Sync>;

/// Spawn a canned Needle server on loopback.
fn spawn_needle_server(handler: Handler) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local address");
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let handler = handler.clone();
            std::thread::spawn(move || serve(stream, handler));
        }
    });
    addr
}

/// Serve exactly one JSON POST per connection, then close.
fn serve(mut stream: TcpStream, handler: Handler) {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        match stream.read(&mut chunk) {
            Ok(0) => return,
            Ok(read) => {
                buffer.extend_from_slice(&chunk[..read]);
                if let Some(position) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                    break position;
                }
            }
            Err(_) => return,
        }
    };
    let headers = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let content_length: usize = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find_map(|(name, value)| {
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
        })
        .flatten()
        .unwrap_or(0);
    while buffer.len() < header_end + 4 + content_length {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            Err(_) => return,
        }
    }
    let body = String::from_utf8_lossy(&buffer[header_end + 4..]).to_string();
    let path = headers
        .lines()
        .next()
        .and_then(|request| request.split(' ').nth(1))
        .unwrap_or("/")
        .to_string();
    let response_body = handler(&path, &body);
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response_body.len(),
        response_body
    );
    let _ = stream.write_all(response.as_bytes());
}

/// A deterministic embedding handler: one hash-derived 2-vector per
/// requested text.
fn embed_handler() -> Handler {
    Arc::new(|path: &str, body: &str| {
        assert_eq!(path, "/embed", "unexpected request path {path}");
        let value: serde_json::Value = serde_json::from_str(body).expect("embed body is JSON");
        let texts = value["texts"].as_array().expect("texts");
        let embeddings: Vec<[f32; 2]> = texts
            .iter()
            .map(|text| {
                let text = text.as_str().unwrap_or("");
                let sum: u32 = text.bytes().map(u32::from).sum();
                [((sum % 97) + 1) as f32, ((text.len() % 13) + 1) as f32]
            })
            .collect();
        serde_json::json!({ "model": "test-mini", "embeddings": embeddings }).to_string()
    })
}

/// A routing handler proposing one fixed specialist score.
fn route_handler(reviewer_id: &'static str, score: f64) -> Handler {
    Arc::new(move |path: &str, _body: &str| {
        assert_eq!(path, "/route", "unexpected request path {path}");
        serde_json::json!({ "reviewers": [{ "reviewer_id": reviewer_id, "score": score }] })
            .to_string()
    })
}

/// A plain directory that is not a Git repository.
fn scratch(label: &str) -> (TempDir, std::path::PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix(&format!("nisse-eval-{label}-"))
        .tempdir()
        .unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    (dir, cwd)
}

fn home() -> TempDir {
    TempDir::new().unwrap()
}

/// Extract the machine-readable JSON of an eval report.
fn machine_readable(stdout: &str) -> serde_json::Value {
    let json = stdout
        .split("machine-readable:\n")
        .nth(1)
        .expect("report carries the machine-readable marker")
        .lines()
        .next()
        .expect("one compact JSON line");
    serde_json::from_str(json).expect("machine-readable report is JSON")
}

/// Recursively drop every `latency*` field so two runs can be compared
/// (latency legitimately varies).
fn strip_latencies(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, val) in map {
                if key.starts_with("latency") {
                    continue;
                }
                out.insert(key.clone(), strip_latencies(val));
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(strip_latencies).collect())
        }
        other => other.clone(),
    }
}

#[test]
fn eval_routing_runs_outside_a_repository_and_emits_json() {
    let (_dir, cwd) = scratch("routing-norepo");
    let home = home();

    let out = run_nisse_in_with_env(&cwd, &["eval", "routing"], home.path(), &[]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert_eq!(stderr, "", "a passing eval must not write to stderr");
    assert!(stdout.starts_with("nisse eval routing\n"));
    assert!(stdout.contains("variant: rules (mandatory baseline)"));
    assert!(stdout.contains("verdict: pass"));
    assert!(!stdout.contains("provider: "), "no provider is configured");

    let report = machine_readable(&stdout);
    assert_eq!(report["evaluation"], "routing");
    assert_eq!(report["variants"].as_array().unwrap().len(), 1);
    let variant = &report["variants"][0];
    assert_eq!(variant["label"], "rules");
    assert_eq!(variant["mandatory"], true);
    assert_eq!(variant["passed"], true);
    assert_eq!(variant["fixtures_passed"], variant["fixtures_total"]);
    assert_eq!(variant["snapshot_leakage"], serde_json::Value::Null);
    assert!(variant["fixtures_total"].as_u64().unwrap() >= 7);
}

#[test]
fn eval_routing_is_deterministic_across_runs() {
    let (_dir, cwd) = scratch("routing-stable");
    let home = home();

    let first = run_nisse_in_with_env(&cwd, &["eval", "routing"], home.path(), &[]);
    let second = run_nisse_in_with_env(&cwd, &["eval", "routing"], home.path(), &[]);
    assert!(first.status.success());
    assert!(second.status.success());

    let first = strip_latencies(&machine_readable(&String::from_utf8(first.stdout).unwrap()));
    let second = strip_latencies(&machine_readable(
        &String::from_utf8(second.stdout).unwrap(),
    ));
    assert_eq!(first, second, "the rules baseline must be deterministic");
}

#[test]
fn eval_routing_reports_the_needle_comparison_variant() {
    let (_dir, cwd) = scratch("routing-needle");
    let home = home();
    let addr = spawn_needle_server(route_handler("testing", 0.9));
    write_user_config(
        home.path(),
        &format!(
            "[classifier]\nprovider = \"needle\"\nendpoint = \"http://127.0.0.1:{}\"\n",
            addr.port()
        ),
    );

    let out = run_nisse_in_with_env(&cwd, &["eval", "routing"], home.path(), &[]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    // The exit status follows the mandatory rules baseline; the
    // provider variant is comparison data.
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");

    let report = machine_readable(&stdout);
    let variants = report["variants"].as_array().unwrap();
    assert_eq!(variants.len(), 2, "rules plus provider comparison");
    assert_eq!(variants[0]["label"], "rules");
    assert_eq!(variants[0]["mandatory"], true);
    assert_eq!(variants[0]["passed"], true);
    assert_eq!(variants[1]["label"], "rules+provider");
    assert_eq!(variants[1]["provider"], "needle");
    assert_eq!(variants[1]["mandatory"], false);

    // The mandatory baseline text precedes the comparison variant and
    // both are reported.
    assert!(stdout.contains("variant: rules (mandatory baseline)"));
    assert!(stdout.contains("variant: rules+provider (provider: needle)"));
    assert!(stdout.contains("exit status follows the mandatory baseline"));
}

#[test]
fn eval_retrieval_passes_baseline_outside_a_repository() {
    let (_dir, cwd) = scratch("retrieval-norepo");
    let home = home();

    let out = run_nisse_in_with_env(&cwd, &["eval", "retrieval"], home.path(), &[]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert_eq!(stderr, "");
    assert!(stdout.starts_with("nisse eval retrieval\n"));
    assert!(stdout.contains("variant: baseline (mandatory baseline)"));
    assert!(stdout.contains("snapshot-isolation"));
    assert!(stdout.contains("snapshot leakage: 0"));
    assert!(stdout.contains("verdict: pass"));
    assert!(!stdout.contains("provider: "), "no provider is configured");

    let report = machine_readable(&stdout);
    assert_eq!(report["evaluation"], "retrieval");
    let variants = report["variants"].as_array().unwrap();
    assert_eq!(variants.len(), 1);
    let variant = &variants[0];
    assert_eq!(variant["label"], "baseline");
    assert_eq!(variant["mandatory"], true);
    assert_eq!(variant["passed"], true);
    assert_eq!(variant["snapshot_leakage"], 0);
    assert_eq!(variant["mean_recall_at_k"].as_f64().unwrap(), 1.0);

    // Leakage counted as failure is visible per fixture: every fixture
    // reports zero leakage and passes.
    for fixture in variant["fixtures"].as_array().unwrap() {
        assert_eq!(fixture["snapshot_leakage"], 0);
        assert_eq!(fixture["passed"], true);
        assert_eq!(fixture["recall_at_k"].as_f64().unwrap(), 1.0);
    }
}

#[test]
fn eval_retrieval_reports_the_embeddings_comparison_variant() {
    let (_dir, cwd) = scratch("retrieval-embed");
    let home = home();
    let addr = spawn_needle_server(embed_handler());
    write_user_config(
        home.path(),
        &format!(
            "[embeddings]\nprovider = \"needle\"\nendpoint = \"http://127.0.0.1:{}\"\n",
            addr.port()
        ),
    );

    let out = run_nisse_in_with_env(&cwd, &["eval", "retrieval"], home.path(), &[]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");

    let report = machine_readable(&stdout);
    let variants = report["variants"].as_array().unwrap();
    assert_eq!(variants.len(), 2, "baseline plus embeddings comparison");
    assert_eq!(variants[0]["label"], "baseline");
    assert_eq!(variants[0]["passed"], true);
    assert_eq!(variants[1]["label"], "embeddings");
    assert_eq!(variants[1]["provider"], "needle");
    assert_eq!(variants[1]["mandatory"], false);
    // Reranking is optional refinement; the correctness gates hold.
    assert_eq!(variants[1]["passed"], true);
    assert_eq!(variants[1]["snapshot_leakage"], 0);
    assert!(stdout.contains("variant: embeddings (provider: needle)"));
}

#[test]
fn eval_reports_do_not_touch_the_current_directory() {
    // Evaluation works from any directory and leaves no state behind:
    // the fixture repositories live in temporary directories.
    let (_dir, cwd) = scratch("retrieval-clean");
    let home = home();

    let out = run_nisse_in_with_env(&cwd, &["eval", "retrieval"], home.path(), &[]);
    assert!(out.status.success());
    let entries: Vec<_> = std::fs::read_dir(&cwd)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .collect();
    assert!(
        entries.is_empty(),
        "eval must not write into the current directory: {entries:?}"
    );
    assert!(
        !Path::new(&cwd).join(".git").exists(),
        "eval must not create a repository in the current directory"
    );
}
