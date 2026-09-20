//! Binary-level integration tests for the optional Needle providers
//! (plan milestone 17).
//!
//! A canned HTTP server on loopback plays the local Needle service, so
//! the real `nisse` binary exercises the real HTTP adapter end to end
//! without any network access beyond loopback:
//!
//! - `nisse query` uses configured embeddings and caches them in the
//!   shared cache (the second query stops re-embedding entries);
//! - `nisse review plan` blends configured routing scores into the
//!   rules decision while the frozen jury constraints hold;
//! - a dead or untrusted endpoint degrades to rules-only routing with
//!   a recorded diagnostic — Needle is never mandatory.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::{run_nisse_in_with_env, write_user_config};
use tempfile::TempDir;

/// A plan whose rules routing selects exactly [quality, concurrency].
const QUEUE_PLAN: &str = "\
# Fix the race on the shared queue

## Approach

Move queue mutation behind a mutex and document the locking order.

## Tests

Add a regression test for concurrent enqueue.
";

type Handler = Arc<dyn Fn(&str, &str) -> String + Send + Sync>;

/// Spawn a canned Needle server on loopback. Returns its address and a
/// counter of served requests.
fn spawn_needle_server(handler: Handler) -> (std::net::SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local address");
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = requests.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let handler = handler.clone();
            let requests = counter.clone();
            std::thread::spawn(move || serve(stream, handler, requests));
        }
    });
    (addr, requests)
}

/// Serve exactly one JSON POST per connection, then close.
fn serve(mut stream: TcpStream, handler: Handler, requests: Arc<AtomicUsize>) {
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

    requests.fetch_add(1, Ordering::SeqCst);
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

/// A port that is (almost certainly) not listening.
fn dead_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    port
}

/// A repository with one indexed Rust source file.
fn rust_repo(label: &str) -> (TempDir, PathBuf, TempDir) {
    let dir = tempfile::Builder::new()
        .prefix(label)
        .tempdir()
        .expect("temp dir");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).expect("create src");
    std::fs::write(
        repo.join("src").join("queue.rs"),
        "pub struct Queue {\n    slots: [u8; 32],\n}\n\npub fn enqueue(queue: &mut Queue, item: u8) {\n    queue.slots[0] = item;\n}\n",
    )
    .expect("write source");
    git(&repo, &["init", "--quiet"]);
    let home = TempDir::new().expect("home");
    (dir, repo, home)
}

fn git(repo: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("LC_ALL", "C")
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}

/// The number of rows in a table of the shared cache.
fn shared_cache_count(repo: &Path, table: &str) -> i64 {
    let db = repo.join(".git/nisse/cache.sqlite");
    let conn = rusqlite::Connection::open(&db).expect("open shared cache");
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
        row.get(0)
    })
    .expect("count rows")
}

/// The persisted review event payload of the latest review.
fn review_event_payload(repo: &Path) -> serde_json::Value {
    let db = repo.join(".git/nisse/cache.sqlite");
    let conn = rusqlite::Connection::open(&db).expect("open shared cache");
    let payload: String = conn
        .query_row(
            "SELECT payload FROM review_event ORDER BY created_at DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .expect("read review event");
    serde_json::from_str(&payload).expect("event payload is JSON")
}

fn jury_ids(event: &serde_json::Value) -> Vec<String> {
    event["jury"]["seats"]
        .as_array()
        .expect("jury seats")
        .iter()
        .map(|seat| seat["reviewer_id"].as_str().expect("seat id").to_string())
        .collect()
}

#[test]
fn query_uses_needle_embeddings_and_caches_vectors() {
    let (_dir, repo, home) = rust_repo("needle-query");
    let (addr, requests) = spawn_needle_server(embed_handler());
    write_user_config(
        home.path(),
        &format!(
            "[embeddings]\nprovider = \"needle\"\nendpoint = \"http://127.0.0.1:{}\"\n",
            addr.port()
        ),
    );

    let out = run_nisse_in_with_env(&repo, &["query", "queue enqueue"], home.path(), &[]);
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    let stderr = String::from_utf8(out.stderr).expect("stderr utf8");
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("nisse query"), "{stdout}");

    // Vectors were cached in the shared cache under the frozen tables.
    let vectors = shared_cache_count(&repo, "embedding");
    assert!(vectors >= 1, "embedding rows: {vectors}");
    let chunks = shared_cache_count(&repo, "text_chunk");
    assert!(chunks >= 1);
    let after_first = requests.load(Ordering::SeqCst);
    assert!(
        after_first >= 2,
        "query call plus entry batch: {after_first}"
    );

    // The identical query re-uses the cached entry vectors: only the
    // model-discovery call remains, and no new rows appear.
    let out = run_nisse_in_with_env(&repo, &["query", "queue enqueue"], home.path(), &[]);
    assert!(
        out.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let after_second = requests.load(Ordering::SeqCst);
    assert_eq!(after_second - after_first, 1, "only the query is embedded");
    assert_eq!(shared_cache_count(&repo, "embedding"), vectors);

    // Without any configured provider the query works identically
    // offline (rules baseline; no requests).
    let offline_home = TempDir::new().expect("home");
    let out = run_nisse_in_with_env(&repo, &["query", "queue enqueue"], offline_home.path(), &[]);
    assert!(out.status.success());
    assert_eq!(requests.load(Ordering::SeqCst), after_second);
}

#[test]
fn plan_review_blends_needle_routing_scores() {
    let (_dir, repo, home) = rust_repo("needle-route");
    let (addr, _requests) = spawn_needle_server(route_handler("testing", 0.9));
    write_user_config(
        home.path(),
        &format!(
            "[classifier]\nprovider = \"needle\"\nendpoint = \"http://127.0.0.1:{}\"\n",
            addr.port()
        ),
    );

    std::fs::write(repo.join("plan.md"), QUEUE_PLAN).expect("write plan");
    let out = run_nisse_in_with_env(
        &repo,
        &["review", "plan", "--file", "plan.md"],
        home.path(),
        &[],
    );
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    let stderr = String::from_utf8(out.stderr).expect("stderr utf8");
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");

    // The provider proposal lifted `testing` into the jury alongside
    // the rules-selected concurrency specialist; the frozen bounds
    // hold (quality first, at most three specialists).
    let event = review_event_payload(&repo);
    assert_eq!(
        jury_ids(&event),
        vec!["quality", "concurrency", "testing"],
        "jury must blend the provider proposal"
    );
    assert_eq!(event["decision"]["dynamic_fallback"], false);
    let diagnostics = event["diagnostics"].as_array().expect("diagnostics");
    assert!(
        !diagnostics
            .iter()
            .any(|d| d.as_str().is_some_and(|s| s.contains("needle"))),
        "no fallback expected: {diagnostics:?}"
    );
}

#[test]
fn plan_review_falls_back_to_rules_when_needle_is_down() {
    let (_dir, repo, home) = rust_repo("needle-down");
    write_user_config(
        home.path(),
        &format!(
            "[classifier]\nprovider = \"needle\"\nendpoint = \"http://127.0.0.1:{}\"\n",
            dead_port()
        ),
    );

    std::fs::write(repo.join("plan.md"), QUEUE_PLAN).expect("write plan");
    let out = run_nisse_in_with_env(
        &repo,
        &["review", "plan", "--file", "plan.md"],
        home.path(),
        &[],
    );
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    let stderr = String::from_utf8(out.stderr).expect("stderr utf8");
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");

    // The review completed with the untouched rules decision and
    // recorded the provider fallback.
    let event = review_event_payload(&repo);
    assert_eq!(jury_ids(&event), vec!["quality", "concurrency"]);
    let diagnostics = event["diagnostics"].as_array().expect("diagnostics");
    assert!(
        diagnostics.iter().any(|d| d
            .as_str()
            .is_some_and(|message| message.contains("needle routing unavailable"))),
        "diagnostics: {diagnostics:?}"
    );
}

#[test]
fn plan_review_refuses_untrusted_needle_endpoints() {
    let (_dir, repo, home) = rust_repo("needle-untrusted");
    // TEST-NET-1 is outside the default-permitted address classes; the
    // trust check fails before any request is attempted.
    write_user_config(
        home.path(),
        "[classifier]\nprovider = \"needle\"\nendpoint = \"http://192.0.2.55:8080\"\n",
    );

    std::fs::write(repo.join("plan.md"), QUEUE_PLAN).expect("write plan");
    let out = run_nisse_in_with_env(
        &repo,
        &["review", "plan", "--file", "plan.md"],
        home.path(),
        &[],
    );
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    let stderr = String::from_utf8(out.stderr).expect("stderr utf8");
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");

    let event = review_event_payload(&repo);
    assert_eq!(jury_ids(&event), vec!["quality", "concurrency"]);
    let diagnostics = event["diagnostics"].as_array().expect("diagnostics");
    assert!(
        diagnostics.iter().any(|d| d
            .as_str()
            .is_some_and(|message| message.contains("needle routing unavailable")
                && message.contains("not trusted"))),
        "diagnostics: {diagnostics:?}"
    );
}

#[test]
fn plan_review_with_needle_embeddings_still_succeeds() {
    let (_dir, repo, home) = rust_repo("needle-review-embed");
    let (addr, _requests) = spawn_needle_server(embed_handler());
    write_user_config(
        home.path(),
        &format!(
            "[embeddings]\nprovider = \"needle\"\nendpoint = \"http://127.0.0.1:{}\"\n",
            addr.port()
        ),
    );

    std::fs::write(repo.join("plan.md"), QUEUE_PLAN).expect("write plan");
    let out = run_nisse_in_with_env(
        &repo,
        &["review", "plan", "--file", "plan.md"],
        home.path(),
        &[],
    );
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    let stderr = String::from_utf8(out.stderr).expect("stderr utf8");
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");

    // The review retrieved context through the optional reranker and
    // the vectors landed in the shared cache.
    assert!(shared_cache_count(&repo, "embedding") >= 1);
    let event = review_event_payload(&repo);
    assert_eq!(jury_ids(&event), vec!["quality", "concurrency"]);
}
