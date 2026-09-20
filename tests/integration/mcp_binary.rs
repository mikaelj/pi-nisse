//! Binary-level integration tests for `nisse mcp` (plan milestone 26).
//!
//! The MCP stdio server is exercised end to end through the real
//! `nisse` binary: JSON-RPC initialize negotiation, the frozen tool
//! surface, mocked (fake-runtime) plan/code reviews, project query,
//! feedback with durable observations, error semantics, and stdout
//! purity — stdout carries newline-delimited protocol messages only,
//! while logs go to stderr.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::run_nisse_in_with_env;
use tempfile::TempDir;

/// A rust plan that routes to the jury [quality, concurrency].
const QUEUE_PLAN: &str = "\
# Fix the race on the shared queue

## Approach

Move queue mutation behind a mutex and document the locking order.
";

const QUEUE_RS: &str = "pub struct Queue {\n    slots: [u8; 32],\n}\n\npub fn enqueue(queue: &mut Queue, item: u8) {\n    queue.slots[0] = item;\n}\n";

const QUEUE_RS_V2: &str = "pub struct Queue {\n    slots: [u8; 32],\n}\n\npub fn enqueue(queue: &mut Queue, item: u8) {\n    queue.slots[0] = item;\n}\n\npub fn drain(queue: &mut Queue) -> u8 {\n    queue.slots[0]\n}\n";

/// The frozen MCP tool surface (plan section 67).
const MCP_TOOLS: [&str; 4] = [
    "query_project",
    "record_feedback",
    "review_code",
    "review_plan",
];

/// Upper bound for one MCP round trip: fake-runtime reviews spawn the
/// Pi host child process, so they take seconds, not milliseconds.
const ROUND_TRIP: Duration = Duration::from_secs(120);

/// A line-based MCP client over one `nisse mcp` child process.
struct McpSession {
    child: Child,
    stdin: Option<ChildStdin>,
    receiver: Receiver<String>,
    /// Everything the server ever wrote to stdout (purity checks).
    stdout_log: Arc<Mutex<String>>,
    next_id: u64,
}

impl Drop for McpSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl McpSession {
    /// Spawn `nisse mcp` in `repo` with the fake reviewer runtime and
    /// an isolated home.
    fn spawn(repo: &Path, home: &TempDir, extra_env: &[(&str, &str)]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_nisse"));
        command
            .arg("mcp")
            .current_dir(repo)
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path().join("xdg"))
            .env("APPDATA", home.path().join("appdata"))
            .env("RUST_LOG", "off")
            .env("NISSE_PI_HOST_RUNTIME", "fake");
        for (key, value) in extra_env {
            command.env(key, value);
        }
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        let mut child = command.spawn().expect("spawn nisse mcp");

        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let stdout_log = Arc::new(Mutex::new(String::new()));
        let (sender, receiver) = channel::<String>();

        // Reader thread: protocol lines to the channel, everything to
        // the accumulated stdout log. The server never writes
        // diagnostics to stdout; stderr is drained separately.
        spawn_stdout_reader(stdout, sender, stdout_log.clone());
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                match line {
                    Ok(line) => eprintln!("nisse-mcp stderr: {line}"),
                    Err(_) => break,
                }
            }
        });

        McpSession {
            stdin: Some(child.stdin.take().expect("piped stdin")),
            child,
            receiver,
            stdout_log,
            next_id: 0,
        }
    }

    fn send(&mut self, value: &serde_json::Value) {
        let line = serde_json::to_string(value).expect("request JSON");
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{line}").expect("write request");
        stdin.flush().expect("flush request");
    }

    /// Read the next stdout line that carries a response for `id`.
    fn receive(&mut self, id: &str) -> serde_json::Value {
        let deadline = std::time::Instant::now() + ROUND_TRIP;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match self.receiver.recv_timeout(remaining) {
                Ok(line) => {
                    let message: serde_json::Value =
                        serde_json::from_str(&line).expect("stdout line is JSON");
                    if message.get("id").and_then(serde_json::Value::as_str) == Some(id) {
                        return message;
                    }
                    // Notifications and other responses are skipped.
                }
                Err(RecvTimeoutError::Timeout) => {
                    panic!("timed out waiting for the response to id {id}")
                }
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("the server closed stdout before answering id {id}")
                }
            }
        }
    }

    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.next_id += 1;
        let id = self.next_id.to_string();
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.send(&request);
        self.receive(&id)
    }

    fn notify(&mut self, method: &str) {
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
        });
        self.send(&notification);
    }

    /// Complete the MCP handshake and return the initialize result.
    fn initialize(&mut self) -> serde_json::Value {
        let response = self.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "nisse-test", "version": "0"},
            }),
        );
        assert!(
            response.get("error").is_none(),
            "initialize failed: {response}"
        );
        self.notify("notifications/initialized");
        response["result"].clone()
    }

    /// `tools/call` returning the full response object.
    fn call_tool_raw(&mut self, name: &str, arguments: serde_json::Value) -> serde_json::Value {
        self.request(
            "tools/call",
            serde_json::json!({"name": name, "arguments": arguments}),
        )
    }

    /// `tools/call` returning the parsed JSON content of a successful
    /// (non-`isError`) result.
    fn call_tool_json(&mut self, name: &str, arguments: serde_json::Value) -> serde_json::Value {
        let response = self.call_tool_raw(name, arguments);
        assert!(
            response.get("error").is_none(),
            "unexpected protocol error: {response}"
        );
        let result = &response["result"];
        assert_ne!(
            result.get("isError").and_then(serde_json::Value::as_bool),
            Some(true),
            "unexpected tool error: {result}"
        );
        let text = result["content"][0]["text"].as_str().expect("text content");
        serde_json::from_str(text).expect("strict JSON tool result")
    }

    /// Assert that everything the server wrote to stdout so far is
    /// newline-delimited JSON (protocol purity) and return it.
    fn stdout_so_far(&self) -> String {
        self.stdout_log.lock().expect("stdout log").clone()
    }

    /// End the session: close stdin (a clean shutdown), wait bounded
    /// for the server to exit and drain its remaining stdout, and
    /// return everything it ever wrote. A hanging server is killed by
    /// `Drop`.
    fn finish(mut self) -> String {
        self.stdin = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match self.receiver.recv_timeout(remaining) {
                // Remaining lines are already in the stdout log.
                Ok(_) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => break,
            }
        }
        self.stdout_so_far()
    }
}

fn spawn_stdout_reader(
    stdout: std::process::ChildStdout,
    sender: Sender<String>,
    log: Arc<Mutex<String>>,
) {
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    let mut log = log.lock().expect("stdout log");
                    log.push_str(&line);
                    log.push('\n');
                    drop(log);
                    if sender.send(line).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });
}

/// Create a repository with one indexed, uncommitted rust source file.
fn rust_repo(label: &str) -> (TempDir, PathBuf, TempDir) {
    let dir = tempfile::Builder::new()
        .prefix(label)
        .tempdir()
        .expect("temp dir");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).expect("create src");
    std::fs::write(repo.join("src").join("queue.rs"), QUEUE_RS).expect("write source");
    git(&repo, &["init", "--quiet"]);
    let home = TempDir::new().expect("home");
    (dir, repo, home)
}

/// Create a repository with one committed rust source file and an
/// unstaged working-tree modification of it.
fn dirty_rust_repo(label: &str) -> (TempDir, PathBuf, TempDir) {
    let (dir, repo, home) = rust_repo(label);
    git(&repo, &["add", "--all"]);
    git(
        &repo,
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=T",
            "commit",
            "--quiet",
            "-m",
            "one",
        ],
    );
    std::fs::write(repo.join("src").join("queue.rs"), QUEUE_RS_V2).expect("modify source");
    (dir, repo, home)
}

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("LC_ALL", "C")
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}

/// The durable observation records of `repo`.
fn observation_files(repo: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let people = repo.join(".nisse/people");
    let Ok(contributors) = std::fs::read_dir(&people) else {
        return files;
    };
    for contributor in contributors {
        let dir = contributor.expect("dir entry").path();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

#[test]
fn initialize_negotiates_and_lists_the_frozen_four_tools() {
    let (_dir, repo, home) = rust_repo("mcp-init");
    let mut session = McpSession::spawn(&repo, &home, &[]);

    let init = session.initialize();
    assert_eq!(init["protocolVersion"], "2025-06-18");
    assert_eq!(init["serverInfo"]["name"], "nisse");
    assert!(
        init["capabilities"]["tools"].is_object(),
        "tools capability: {init}"
    );

    let response = session.request("tools/list", serde_json::json!({}));
    assert!(response.get("error").is_none(), "{response}");
    let tools = response["result"]["tools"].as_array().expect("tools");
    let mut names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(names, MCP_TOOLS);

    // Every tool advertises a JSON Schema input.
    for tool in tools {
        let schema = &tool["inputSchema"];
        assert_eq!(
            schema["type"], "object",
            "{:?} schema: {schema}",
            tool["name"]
        );
    }
    // The review_code schema names the frozen scopes and their camelCase
    // companion fields.
    let review_code = tools
        .iter()
        .find(|tool| tool["name"] == "review_code")
        .expect("review_code");
    let schema_text = serde_json::to_string(&review_code["inputSchema"]).unwrap();
    for scope in ["working", "staged", "commit", "range", "branch"] {
        assert!(schema_text.contains(scope), "scope {scope} missing");
    }
    for field in ["baseRef", "commit", "range"] {
        assert!(schema_text.contains(field), "field {field} missing");
    }

    // Stdout purity: everything the server wrote is newline-delimited
    // JSON-RPC (one message per line, nothing else).
    let stdout = session.finish();
    for line in stdout.lines() {
        assert!(
            serde_json::from_str::<serde_json::Value>(line).is_ok(),
            "stdout is not pure JSON-RPC: {line:?}"
        );
    }
}

#[test]
fn review_plan_tool_runs_the_mocked_jury_like_the_cli() {
    let (_dir, repo, home) = rust_repo("mcp-plan");
    let mut session = McpSession::spawn(&repo, &home, &[]);
    session.initialize();

    let result = session.call_tool_json("review_plan", serde_json::json!({ "plan": QUEUE_PLAN }));
    assert_eq!(result["kind"], "plan");
    assert_eq!(result["snapshot"]["mode"], "working");
    assert!(
        result["review_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "{result}"
    );
    let findings = result["findings"].as_array().expect("findings");
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0]["severity"], "info");
    assert_eq!(findings[0]["cross_validated"], true);
    assert_eq!(
        findings[0]["reviewer_ids"],
        serde_json::json!(["concurrency", "quality"])
    );

    // Same core behavior as the CLI: the identical plan reviewed via
    // `nisse review plan --json --file <plan outside the worktree>`
    // yields the same result shape (the shared core operation differs
    // only in the generated ULIDs; the plan outside the worktree keeps
    // the inputs identical, including the absent plan path).
    let plan_file = repo.parent().unwrap().join("plan.md");
    std::fs::write(&plan_file, QUEUE_PLAN).expect("write plan");
    let out = run_nisse_in_with_env(
        &repo,
        &[
            "review",
            "plan",
            "--json",
            "--file",
            plan_file.to_str().unwrap(),
        ],
        home.path(),
        &[],
    );
    assert!(
        out.status.success(),
        "stdout: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let cli: serde_json::Value =
        serde_json::from_str(&String::from_utf8(out.stdout).expect("stdout utf8")).unwrap();
    assert_eq!(cli["kind"], result["kind"]);
    assert_eq!(cli["snapshot"], result["snapshot"]);
    assert_eq!(cli["summary"], result["summary"]);
    assert_eq!(cli["findings"].as_array().unwrap().len(), findings.len());
    assert_eq!(cli["findings"][0]["severity"], findings[0]["severity"]);
    assert_eq!(cli["findings"][0]["claim"], findings[0]["claim"]);
    assert_eq!(
        cli["findings"][0]["reviewer_ids"],
        findings[0]["reviewer_ids"]
    );
    assert_ne!(
        cli["review_id"], result["review_id"],
        "each review has its own identity"
    );
    session.finish();
}

#[test]
fn review_code_tool_reviews_the_working_diff() {
    let (_dir, repo, home) = dirty_rust_repo("mcp-code");
    let mut session = McpSession::spawn(&repo, &home, &[]);
    session.initialize();

    let result = session.call_tool_json("review_code", serde_json::json!({ "scope": "working" }));
    assert_eq!(result["kind"], "code");
    assert_eq!(result["snapshot"]["mode"], "working");

    // The staged mode reviews the index (no staged changes here), so
    // it fails explicitly as a tool error with the CLI's message.
    let response = session.call_tool_raw("review_code", serde_json::json!({ "scope": "staged" }));
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"]["isError"], true);
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("no changes to review"),
        "tool error text: {text}"
    );
    session.finish();
}

#[test]
fn query_project_tool_returns_the_shared_report() {
    let (_dir, repo, home) = rust_repo("mcp-query");
    let mut session = McpSession::spawn(&repo, &home, &[]);
    session.initialize();

    let report = session.call_tool_json("query_project", serde_json::json!({ "query": "queue" }));
    assert_eq!(report["query"], "queue");
    assert_eq!(report["snapshot"]["mode"], "working");
    let entries = report["entries"].as_array().expect("entries");
    assert!(!entries.is_empty());
    assert!(
        entries.iter().any(|entry| entry["path"] == "src/queue.rs"),
        "entries: {entries:?}"
    );
    session.finish();
}

#[test]
fn record_feedback_tool_creates_observations_and_is_idempotent() {
    let (_dir, repo, home) = rust_repo("mcp-feedback");
    super::write_user_config(home.path(), "[contributor]\nid = \"alice\"\n");
    let mut session = McpSession::spawn(&repo, &home, &[]);
    session.initialize();

    // A stored review to feed back on.
    let review = session.call_tool_json("review_plan", serde_json::json!({ "plan": QUEUE_PLAN }));
    let review_id = review["review_id"].as_str().expect("review id").to_string();
    let finding_id = review["findings"][0]["id"]
        .as_str()
        .expect("finding id")
        .to_string();

    let report = session.call_tool_json(
        "record_feedback",
        serde_json::json!({
            "reviewId": review_id,
            "findingId": finding_id,
            "outcome": "fixed",
        }),
    );
    assert_eq!(report["reviewId"], review_id);
    assert_eq!(report["findingId"], finding_id);
    assert_eq!(report["outcome"], "fixed");
    assert_eq!(report["status"], "recorded");
    // One immutable observation per contributing reviewer.
    let observations = report["observations"].as_array().expect("observations");
    assert_eq!(observations.len(), 2);
    assert_eq!(observation_files(&repo).len(), 2);

    // Repeated submission is an idempotent no-op.
    let again = session.call_tool_json(
        "record_feedback",
        serde_json::json!({
            "reviewId": review_id,
            "findingId": finding_id,
            "outcome": "fixed",
        }),
    );
    assert_eq!(again["status"], "already-recorded");
    assert_eq!(observation_files(&repo).len(), 2);

    // An unknown finding is a tool error carrying the typed message.
    let response = session.call_tool_raw(
        "record_feedback",
        serde_json::json!({
            "reviewId": review_id,
            "findingId": "no-such-finding",
            "outcome": "fixed",
        }),
    );
    assert_eq!(response["result"]["isError"], true);
    assert!(
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("has no finding"),
        "unknown finding error"
    );
    session.finish();
}

#[test]
fn invalid_input_is_rejected_before_any_nisse_work() {
    let (_dir, repo, home) = rust_repo("mcp-invalid");
    let mut session = McpSession::spawn(&repo, &home, &[]);
    session.initialize();

    // Blank required fields are protocol-level invalid_params errors.
    let blank_plan = session.call_tool_raw("review_plan", serde_json::json!({ "plan": "  " }));
    assert_eq!(blank_plan["error"]["code"], -32602, "{blank_plan}");

    // A scope without its companion revision is invalid too.
    let missing_commit =
        session.call_tool_raw("review_code", serde_json::json!({ "scope": "commit" }));
    assert_eq!(missing_commit["error"]["code"], -32602, "{missing_commit}");
    let missing_branch =
        session.call_tool_raw("review_code", serde_json::json!({ "scope": "branch" }));
    assert_eq!(missing_branch["error"]["code"], -32602, "{missing_branch}");

    // Unknown tools are routing errors, never executed.
    let unknown = session.call_tool_raw("no_such_tool", serde_json::json!({}));
    assert!(unknown.get("error").is_some(), "{unknown}");
    session.finish();
}

#[test]
fn failed_reviews_are_explicit_tool_errors() {
    let (_dir, repo, home) = rust_repo("mcp-fail");
    let mut session =
        McpSession::spawn(&repo, &home, &[("NISSE_FAKE_REVIEWER_FAIL_FOR", "quality")]);
    session.initialize();

    // The fake jury loses its quality reviewer: the review fails
    // explicitly (fewer than two valid reports) as an isError tool
    // result carrying the CLI's failure text, and the protocol stays
    // usable afterwards.
    let response = session.call_tool_raw("review_plan", serde_json::json!({ "plan": QUEUE_PLAN }));
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"]["isError"], true);
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("at least 2 are required"), "text: {text}");
    assert!(text.contains("reviewer `quality` failed"), "text: {text}");

    // The server keeps serving: a deterministic query still works.
    let report = session.call_tool_json("query_project", serde_json::json!({ "query": "queue" }));
    assert!(!report["entries"].as_array().unwrap().is_empty());
    session.finish();
}

#[test]
fn stdout_carries_protocol_only_throughout_a_busy_session() {
    let (_dir, repo, home) = dirty_rust_repo("mcp-purity");
    super::write_user_config(home.path(), "[contributor]\nid = \"alice\"\n");
    let mut session = McpSession::spawn(&repo, &home, &[]);
    session.initialize();

    // A busy session: every kind of tool, including two reviews that
    // spawn the Pi host child process and write local caches.
    let review = session.call_tool_json("review_plan", serde_json::json!({ "plan": QUEUE_PLAN }));
    let code = session.call_tool_json("review_code", serde_json::json!({ "scope": "working" }));
    assert_eq!(code["kind"], "code");
    let query = session.call_tool_json("query_project", serde_json::json!({ "query": "queue" }));
    assert!(!query["entries"].as_array().unwrap().is_empty());
    let feedback = session.call_tool_json(
        "record_feedback",
        serde_json::json!({
            "reviewId": review["review_id"],
            "findingId": review["findings"][0]["id"],
            "outcome": "accepted",
        }),
    );
    assert_eq!(feedback["status"], "recorded");

    // Stdout purity: every line ever written is exactly one JSON-RPC
    // message with an id or a method — no logs, no review text, no
    // interleaved child-process output.
    let stdout = session.finish();
    let lines: Vec<&str> = stdout.lines().collect();
    // initialize plus one response per tool call.
    assert!(lines.len() >= 5, "expected the session messages: {stdout}");
    for line in lines {
        let message: serde_json::Value =
            serde_json::from_str(line).expect("every stdout line is JSON");
        assert!(
            message.get("jsonrpc").is_some(),
            "not a JSON-RPC message: {line:?}"
        );
        assert!(
            message.get("id").is_some() || message.get("method").is_some(),
            "neither response nor notification: {line:?}"
        );
    }
}
