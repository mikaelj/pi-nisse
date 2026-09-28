//! Windows-readiness integration tests (plan milestone 27).
//!
//! The Windows audit covers: paths (including spaces and non-ASCII
//! segments), Unicode file names, CRLF file content, Git output
//! handling, SQLite under hostile paths, the child-process JSONL
//! boundary (Pi host), the MCP stdio subprocess, atomic writes, config
//! paths, and linked worktrees.
//!
//! The product code paths are identical on every platform — only path
//! forms differ (Git prints `C:/repo/.git` on Windows while Rust's
//! `canonicalize` yields verbatim `\\?\C:\repo\.git`). These tests
//! therefore run on every platform, including the Windows CI job, from
//! repositories that deliberately combine the hazards: a worktree root
//! with spaces and non-ASCII characters, file names with spaces,
//! non-ASCII file names, CRLF source content, and a linked worktree
//! whose administrative directory name itself retains non-ASCII
//! characters (Git replaces spaces when deriving it).

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::Duration;

use super::run_nisse_in_with_env;
use nisse::domain::context::{ContextBundle, ContextEntry, ContextKind};
use nisse::domain::model::ThinkingLevel;
use nisse::domain::review::ReviewKind;
use nisse::domain::reviewer::{ReviewerCapability, ReviewerExecutionSpec};
use nisse::jury::host::{HostProcess, PiHostLaunch, ReviewerOutcome};
use nisse::jury::protocol::{HostRuntime, StartReviewPayload, ToolResponsePayload};
use tempfile::TempDir;

/// Worktree-root directory name combining spaces, parentheses, and
/// non-ASCII characters.
const HOSTILE_DIR: &str = "nisse répo (τ27)";

/// A queue source file whose name contains a space and whose content
/// uses CRLF line endings throughout (the common Windows state).
const CRLF_QUEUE_RS: &str = "pub struct Queue {\r\n    slots: [u8; 32],\r\n}\r\n\r\npub fn enqueue(queue: &mut Queue, item: u8) {\r\n    queue.slots[0] = item;\r\n}\r\n";

const CRLF_QUEUE_RS_V2: &str = "pub struct Queue {\r\n    slots: [u8; 32],\r\n}\r\n\r\npub fn enqueue(queue: &mut Queue, item: u8) {\r\n    queue.slots[0] = item;\r\n}\r\n\r\npub fn drain(queue: &mut Queue) -> u8 {\r\n    queue.slots[0]\r\n}\r\n";

/// The frozen MCP tool surface (plan section 67).
const MCP_TOOLS: [&str; 5] = [
    "query_project",
    "record_feedback",
    "review_code",
    "review_investigation",
    "review_plan",
];

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("LC_ALL", "C")
        .status()
        .expect("spawn git");
    assert!(
        status.success(),
        "git {args:?} failed in {}",
        repo.display()
    );
}

/// Commit all current changes; returns the new commit OID.
fn commit_all(repo: &Path, message: &str) -> String {
    git(repo, &["add", "--all"]);
    git(
        repo,
        &[
            "-c",
            "user.email=nisse@test.invalid",
            "-c",
            "user.name=Nisse Test",
            "commit",
            "--quiet",
            "-m",
            message,
        ],
    );
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .env("LC_ALL", "C")
        .output()
        .expect("spawn git");
    String::from_utf8(out.stdout)
        .expect("oid utf8")
        .trim()
        .to_string()
}

/// A committed repository under a hostile (spaces + non-ASCII) root
/// containing a spaced CRLF file, a non-ASCII file name, and a CJK file
/// name, plus an isolated home directory.
fn hostile_repo() -> (TempDir, PathBuf, TempDir) {
    let dir = tempfile::Builder::new()
        .prefix("nisse-windows-compat-")
        .tempdir()
        .expect("temp dir");
    let repo = dir.path().join(HOSTILE_DIR);
    std::fs::create_dir_all(repo.join("src")).expect("create src");
    std::fs::write(repo.join("src").join("crlf queue.rs"), CRLF_QUEUE_RS).expect("write crlf");
    std::fs::write(
        repo.join("src").join("räksmørgås.rs"),
        "pub fn smorgasbord() -> u32 {\n    32\n}\n",
    )
    .expect("write unicode name");
    std::fs::write(
        repo.join("src").join("日本語.rs"),
        "pub fn teikisuu() -> u16 {\n    7\n}\n",
    )
    .expect("write cjk name");
    git(&repo, &["init", "--quiet", "-b", "main"]);
    commit_all(&repo, "base");
    let home = TempDir::new().expect("home");
    (dir, repo, home)
}

/// Run one command in `cwd`, returning (ok, stdout, stderr).
fn run_in(cwd: &Path, home: &TempDir, args: &[&str]) -> (bool, String, String) {
    let out = run_nisse_in_with_env(cwd, args, home.path(), &[]);
    (
        out.status.success(),
        String::from_utf8(out.stdout).expect("stdout utf8"),
        String::from_utf8(out.stderr).expect("stderr utf8"),
    )
}

#[test]
fn working_review_and_query_support_spaced_unicode_paths_and_crlf() {
    let (_dir, repo, home) = hostile_repo();
    // Final working-tree state: a CRLF modification of the spaced file
    // plus an untracked file whose name contains a space.
    std::fs::write(repo.join("src").join("crlf queue.rs"), CRLF_QUEUE_RS_V2)
        .expect("modify crlf file");
    std::fs::write(repo.join("src").join("ny fil.rs"), "pub fn ny_fil() {}\n").expect("untracked");

    // The full code-review pipeline under the hostile root: discovery,
    // snapshot resolution, index refresh, routing, the real Pi-host
    // child process (fake reviewer runtime), evidence validation
    // against the exact working-tree snapshot, and the shared SQLite
    // cache under `.git/nisse/`.
    let (ok, stdout, stderr) = run_in(&repo, &home, &["review", "code", "--working"]);
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert_eq!(stderr, "");
    assert!(stdout.contains("snapshot: working"), "{stdout}");
    assert!(stdout.contains("findings"), "{stdout}");
    assert!(
        repo.join(".git")
            .join("nisse")
            .join("cache.sqlite")
            .is_file()
    );

    // Retrieval finds symbols in every hostile file name form: a name
    // with a space and CRLF content, a non-ASCII name, and a CJK name.
    for (query, path) in [
        ("enqueue", "src/crlf queue.rs"),
        ("smorgasbord", "src/räksmørgås.rs"),
        ("teikisuu", "src/日本語.rs"),
    ] {
        let (ok, stdout, stderr) = run_in(&repo, &home, &["query", query, "--json"]);
        assert!(ok, "query `{query}` failed: {stderr}");
        assert!(stdout.contains(path), "query `{query}`: {stdout}");
    }

    // Durable-record listing and generated-cache reporting also work
    // under the hostile root.
    let (ok, _, stderr) = run_in(&repo, &home, &["records", "list"]);
    assert!(ok, "records list failed: {stderr}");
    let (ok, _, stderr) = run_in(&repo, &home, &["cache", "status"]);
    assert!(ok, "cache status failed: {stderr}");
}

#[test]
fn linked_worktrees_under_spaced_paths_stay_isolated() {
    let (_dir, repo, home) = hostile_repo();
    // Both worktrees rewrite the same path with different content.
    std::fs::write(
        repo.join("src").join("shared.rs"),
        "pub fn main_marker() -> u8 {\n    1\n}\n",
    )
    .expect("main marker");
    commit_all(&repo, "diverge base");

    let wt = repo.parent().unwrap().join(format!("{HOSTILE_DIR}-wt"));
    git(
        &repo,
        &[
            "worktree",
            "add",
            "--quiet",
            &wt.to_string_lossy(),
            "-b",
            "topic",
        ],
    );
    std::fs::write(
        wt.join("src").join("shared.rs"),
        "pub fn wt_marker() -> u8 {\n    2\n}\n",
    )
    .expect("wt marker");

    // Each worktree's query sees only its own final working-tree state;
    // the shared cache under the main common dir never leaks content
    // membership across worktrees (permanent invariants 2 and 3).
    let (ok, stdout, stderr) = run_in(&repo, &home, &["query", "main_marker", "--json"]);
    assert!(ok, "{stderr}");
    assert!(stdout.contains("main_marker"), "{stdout}");
    assert!(
        !stdout.contains("wt_marker"),
        "worktree leaked into main: {stdout}"
    );

    let (ok, stdout, stderr) = run_in(&wt, &home, &["query", "wt_marker", "--json"]);
    assert!(ok, "{stderr}");
    assert!(stdout.contains("wt_marker"), "{stdout}");
    assert!(
        !stdout.contains("main_marker"),
        "main leaked into worktree: {stdout}"
    );

    // The shared cache is physical under the main common dir, and the
    // linked worktree's private database lives under its own Git
    // directory — whose administrative directory name still contains
    // non-ASCII characters and parentheses (Git sanitizes spaces).
    // The exact directory is resolved from Git itself rather than
    // reconstructed, because Git derives the administrative name.
    assert!(
        repo.join(".git")
            .join("nisse")
            .join("cache.sqlite")
            .is_file()
    );
    let git_dir = Command::new("git")
        .args(["rev-parse", "--absolute-git-dir"])
        .current_dir(&wt)
        .env("LC_ALL", "C")
        .output()
        .expect("spawn git");
    assert!(git_dir.status.success());
    let wt_git_dir = String::from_utf8(git_dir.stdout)
        .expect("git dir utf8")
        .trim()
        .to_string();
    assert!(
        wt_git_dir.contains('τ'),
        "hostile name sanitized away: {wt_git_dir}"
    );
    let wt_git_db = Path::new(&wt_git_dir).join("nisse").join("worktree.sqlite");
    assert!(
        wt_git_db.is_file(),
        "missing worktree db: {}",
        wt_git_db.display()
    );
}

#[test]
fn pi_host_child_process_runs_in_a_spaced_unicode_directory() {
    let (_dir, repo, _home) = hostile_repo();

    // Spawn the real TypeScript host with its working directory inside
    // the hostile root and drive one scripted fake review to completion:
    // the JSONL boundary, request correlation, and the tool
    // request/response round trip all cross the child-process boundary
    // under hostile paths.
    let mut launch = PiHostLaunch::new(
        "node",
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("runtime/pi-host/src/main.ts"),
        HostRuntime::Fake,
    );
    launch.cwd = Some(repo.clone());
    let mut host = HostProcess::spawn(&launch).expect("spawn failed");
    let ready = host
        .wait_ready(Duration::from_secs(30))
        .expect("host_ready");
    assert_eq!(ready.runtime, HostRuntime::Fake);

    let payload = StartReviewPayload {
        review_id: "rev-windows-compat".to_string(),
        kind: ReviewKind::Plan,
        reviewers: vec![compat_spec()],
    };
    host.start_review(&payload).expect("start_review failed");
    let mut tool_calls = 0usize;
    let outcomes = host
        .complete_review(
            "rev-windows-compat",
            &["quality".to_string()],
            &mut |_request| {
                tool_calls += 1;
                ToolResponsePayload::success("ignored", compat_bundle())
            },
            Duration::from_secs(30),
        )
        .expect("review should complete");
    assert!(tool_calls >= 1, "the fake reviewer probes its tools");
    match &outcomes[0].1 {
        ReviewerOutcome::Completed(report) => {
            assert_eq!(report.reviewer_id, "quality");
            assert_eq!(report.findings.len(), 1);
        }
        other => panic!("expected completion, got {other:?}"),
    }
    host.shutdown(Duration::from_secs(10)).expect("clean exit");
}

/// One reviewer execution spec whose initial context cites the hostile
/// file paths, exercising those paths across the JSONL boundary.
fn compat_spec() -> ReviewerExecutionSpec {
    ReviewerExecutionSpec {
        reviewer_id: "quality".to_string(),
        specialization: "general quality".to_string(),
        instructions: "Work the checklist.".to_string(),
        checklist: vec!["Check error paths.".to_string()],
        model_profile: "standard".to_string(),
        thinking_level: ThinkingLevel::Medium,
        initial_context: vec![ContextEntry {
            kind: ContextKind::SymbolDefinition,
            title: "function enqueue".to_string(),
            path: Some("src/crlf queue.rs".to_string()),
            symbol: Some("enqueue".to_string()),
            line_start: Some(5),
            line_end: Some(7),
            record_id: None,
            practice_id: None,
            text: "pub fn enqueue(queue: &mut Queue, item: u8)".to_string(),
            score: 1.0,
        }],
        allowed_capabilities: vec![
            ReviewerCapability::ProjectLookup,
            ReviewerCapability::SourceLookup,
        ],
        max_lookup_calls: 4,
        max_returned_context_bytes: 65_536,
        timeout_ms: 30_000,
    }
}

fn compat_bundle() -> ContextBundle {
    ContextBundle {
        entries: vec![ContextEntry {
            kind: ContextKind::SourceSkeleton,
            title: "skeleton src/räksmørgås.rs".to_string(),
            path: Some("src/räksmørgås.rs".to_string()),
            symbol: None,
            line_start: None,
            line_end: None,
            record_id: None,
            practice_id: None,
            text: "pub fn smorgasbord() -> u32".to_string(),
            score: 0.5,
        }],
        truncated: false,
        text_bytes: 64,
    }
}

#[test]
fn mcp_subprocess_serves_a_spaced_unicode_repository() {
    let (_dir, repo, home) = hostile_repo();
    let mut session = McpCompatSession::spawn(&repo, home.path());

    let init = session.initialize();
    assert_eq!(init["serverInfo"]["name"], "nisse", "{init}");

    // The frozen tool surface is served over stdio under hostile paths.
    let tools = session.request("tools/list", serde_json::json!({}));
    let mut names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name"))
        .collect();
    names.sort_unstable();
    assert_eq!(names, MCP_TOOLS, "{tools}");

    // A query resolves the CRLF file with the spaced name through the
    // same in-process core operations the CLI uses.
    let result = session.call_tool_json("query_project", serde_json::json!({"query": "enqueue"}));
    let text = serde_json::to_string(&result).expect("serialized result");
    assert!(text.contains("src/crlf queue.rs"), "{text}");

    // stdout purity: every line the server ever wrote is one JSON
    // value; diagnostics went to stderr only.
    let stdout = session.finish();
    for line in stdout.lines() {
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|err| panic!("non-JSON stdout line ({err}): {line}"));
    }
}

/// A minimal line-based MCP stdio client for the compat suite (the
/// full client lives in `mcp_binary.rs`).
struct McpCompatSession {
    child: Child,
    stdin: Option<ChildStdin>,
    receiver: Receiver<String>,
    stdout_log: Arc<Mutex<String>>,
    next_id: u64,
}

impl Drop for McpCompatSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl McpCompatSession {
    fn spawn(repo: &Path, home: &Path) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_nisse"));
        command
            .arg("mcp")
            .current_dir(repo)
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home.join("xdg"))
            .env("APPDATA", home.join("appdata"))
            .env("RUST_LOG", "off")
            .env("NISSE_PI_HOST_RUNTIME", "fake")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("spawn nisse mcp");

        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let stdout_log = Arc::new(Mutex::new(String::new()));
        let (sender, receiver) = channel::<String>();
        {
            let log = Arc::clone(&stdout_log);
            std::thread::spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines() {
                    match line {
                        Ok(line) => {
                            let mut buffer = log.lock().expect("stdout log");
                            buffer.push_str(&line);
                            buffer.push('\n');
                            drop(buffer);
                            if sender.send(line).is_err() {
                                return;
                            }
                        }
                        Err(_) => return,
                    }
                }
            });
        }
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                match line {
                    Ok(line) => eprintln!("nisse-mcp stderr: {line}"),
                    Err(_) => break,
                }
            }
        });

        McpCompatSession {
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

    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.next_id += 1;
        let id = self.next_id.to_string();
        self.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match self.receiver.recv_timeout(remaining) {
                Ok(line) => {
                    let message: serde_json::Value =
                        serde_json::from_str(&line).expect("stdout line is JSON");
                    if message.get("id").and_then(serde_json::Value::as_str) == Some(id.as_str()) {
                        return message;
                    }
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

    fn initialize(&mut self) -> serde_json::Value {
        let response = self.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "nisse-compat", "version": "0"},
            }),
        );
        assert!(
            response.get("error").is_none(),
            "initialize failed: {response}"
        );
        self.send(&serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        response["result"].clone()
    }

    fn call_tool_json(&mut self, name: &str, arguments: serde_json::Value) -> serde_json::Value {
        let response = self.request(
            "tools/call",
            serde_json::json!({"name": name, "arguments": arguments}),
        );
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

    /// Close stdin, wait bounded for the server to exit, and return
    /// everything it ever wrote to stdout.
    fn finish(mut self) -> String {
        self.stdin = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match self.receiver.recv_timeout(remaining) {
                Ok(_) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => break,
            }
        }
        self.stdout_log.lock().expect("stdout log").clone()
    }
}
