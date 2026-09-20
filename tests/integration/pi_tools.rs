//! Integration tests for the Rust side of the read-only reviewer tools
//! (plan milestone 13).
//!
//! [`ReviewerToolServer`] answers the Pi host's `tool_request` messages.
//! These tests prove every tool resolves against the exact target
//! snapshot (historical commits, linked worktrees, untracked working-tree
//! files), and that the deterministic bounds — capability allowlist,
//! lookup count, returned-context budget — are enforced. The final test
//! drives the full loop: a fake-runtime reviewer's tool call is served by
//! this server over the real child-process protocol.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use nisse::domain::context::ContextKind;
use nisse::domain::model::ThinkingLevel;
use nisse::domain::reviewer::{ReviewerCapability, ReviewerExecutionSpec};
use nisse::git::{
    GitRunner, RepoContext, ResolvedSnapshot, SnapshotReader, SnapshotSpec, resolve_snapshot,
};
use nisse::index::refresh_index;
use nisse::jury::host::{HostProcess, PiHostLaunch, ReviewerOutcome};
use nisse::jury::protocol::{
    HostRuntime, StartReviewPayload, ToolRequestPayload, ToolResponsePayload,
};
use nisse::jury::tools::{self, ReviewerToolServer};
use nisse::retrieval::Retriever;
use nisse::storage::StoragePaths;

// ---------------------------------------------------------------------------
// Repository fixtures
// ---------------------------------------------------------------------------

fn git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("LC_ALL", "C")
        .output()
        .expect("failed to spawn git");
    assert!(
        output.status.success(),
        "git {} failed in {}: {}",
        args.join(" "),
        cwd.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_out(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("LC_ALL", "C")
        .output()
        .expect("failed to spawn git");
    assert!(
        output.status.success(),
        "git {} failed in {}: {}",
        args.join(" "),
        cwd.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(
        path.parent()
            .unwrap_or_else(|| panic!("path {relative} has no parent")),
    )
    .unwrap();
    std::fs::write(path, contents).unwrap();
}

fn commit_all(root: &Path, message: &str) -> String {
    git(root, &["add", "--all"]);
    git(root, &["commit", "--quiet", "-m", message]);
    git_out(root, &["rev-parse", "HEAD"])
}

/// A disposable repository with the same deterministic identity the
/// in-crate fixtures use.
struct Repo {
    _dir: tempfile::TempDir,
    root: PathBuf,
    branch: String,
}

impl Repo {
    fn new(label: &str) -> Repo {
        let dir = tempfile::Builder::new()
            .prefix(&format!("nisse-pi-tools-{label}-"))
            .tempdir()
            .expect("tempdir");
        git(dir.path(), &["init", "--quiet", "repo"]);
        let root = dir.path().join("repo").canonicalize().unwrap();
        for (key, value) in [
            ("user.email", "nisse@test.invalid"),
            ("user.name", "Nisse Test"),
            ("commit.gpgsign", "false"),
            ("tag.gpgsign", "false"),
            ("core.hooksPath", ".git/hooks"),
        ] {
            git(&root, &["config", key, value]);
        }
        let branch = {
            let head = std::fs::read_to_string(root.join(".git/HEAD")).unwrap();
            head.trim()
                .strip_prefix("ref: refs/heads/")
                .expect("symbolic HEAD")
                .to_owned()
        };
        Repo {
            _dir: dir,
            root,
            branch,
        }
    }

    /// Path of a sibling directory next to the repository.
    fn sibling(&self, name: &str) -> PathBuf {
        self.root.parent().unwrap().join(name)
    }
}

// ---------------------------------------------------------------------------
// Server fixtures
// ---------------------------------------------------------------------------

/// One resolved, indexed review snapshot.
struct ReviewSetup {
    ctx: RepoContext,
    resolved: ResolvedSnapshot,
    storage: StoragePaths,
}

impl ReviewSetup {
    fn prepare(root: &Path, spec: &SnapshotSpec) -> ReviewSetup {
        let ctx = RepoContext::discover(&GitRunner::new(), root).expect("discovery");
        let resolved = resolve_snapshot(&ctx, spec).expect("snapshot resolution");
        let storage = StoragePaths::for_repo(&ctx);
        refresh_index(&ctx, &resolved, &storage).expect("index refresh");
        ReviewSetup {
            ctx,
            resolved,
            storage,
        }
    }

    /// Build the tool server bound to this exact snapshot.
    fn server(
        &self,
        review_id: &str,
        reviewers: &[ReviewerExecutionSpec],
    ) -> ReviewerToolServer<'_> {
        let retriever = Retriever::open(&self.resolved, &self.storage).expect("retriever");
        let reader = SnapshotReader::new(&self.ctx, &self.resolved).expect("reader");
        ReviewerToolServer::new(review_id, reviewers, retriever, reader)
    }
}

fn spec(
    reviewer_id: &str,
    capabilities: &[ReviewerCapability],
    max_lookup_calls: u32,
    max_returned_context_bytes: u64,
) -> ReviewerExecutionSpec {
    ReviewerExecutionSpec {
        reviewer_id: reviewer_id.to_string(),
        specialization: "general correctness".to_string(),
        instructions: "Work the checklist.".to_string(),
        checklist: vec!["Check error paths.".to_string()],
        model_profile: "standard".to_string(),
        thinking_level: ThinkingLevel::Medium,
        initial_context: Vec::new(),
        allowed_capabilities: capabilities.to_vec(),
        max_lookup_calls,
        max_returned_context_bytes,
        timeout_ms: 30_000,
    }
}

fn request(
    review_id: &str,
    reviewer_id: &str,
    tool: ReviewerCapability,
    arguments: serde_json::Value,
) -> ToolRequestPayload {
    ToolRequestPayload {
        review_id: review_id.to_string(),
        reviewer_id: reviewer_id.to_string(),
        tool,
        arguments,
    }
}

/// Assert a successful response and return its bundle.
fn bundle_of(response: &ToolResponsePayload) -> &nisse::domain::context::ContextBundle {
    assert!(response.ok, "expected success, got {response:?}");
    response.bundle.as_ref().expect("success carries a bundle")
}

/// Assert a failed response with the given code and return its message.
fn failure_of(response: &ToolResponsePayload, code: &str) -> String {
    assert!(!response.ok, "expected failure {code}, got {response:?}");
    let error = response.error.as_ref().expect("failure carries an error");
    assert_eq!(error.code, code, "unexpected failure: {response:?}");
    error.message.clone()
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

#[test]
fn each_tool_serves_bounded_target_snapshot_results() {
    let repo = Repo::new("tools-each");
    write(
        &repo.root,
        "src/lib.rs",
        "pub fn alpha() -> u8 {\n    1\n}\n",
    );
    write(
        &repo.root,
        "src/main.rs",
        "mod lib;\n\nfn main() {\n    let value = alpha();\n    let _ = value;\n}\n",
    );
    commit_all(&repo.root, "one");
    // Untracked files are part of the final working-tree state.
    write(
        &repo.root,
        "src/fresh.rs",
        "pub fn fresh_helper() -> u8 {\n    7\n}\n",
    );

    let setup = ReviewSetup::prepare(&repo.root, &SnapshotSpec::Working);
    let reviewers = [spec("quality", &ReviewerCapability::ALL, 16, 256 * 1024)];
    let mut server = setup.server("rev-tools", &reviewers);

    // project_lookup: ranked bundle over the exact snapshot.
    let response = server.handle(
        "t-1",
        &request(
            "rev-tools",
            "quality",
            ReviewerCapability::ProjectLookup,
            serde_json::json!({"query": "alpha fresh_helper"}),
        ),
    );
    let bundle = bundle_of(&response);
    assert!(
        bundle
            .entries
            .iter()
            .any(|entry| entry.kind == ContextKind::SymbolDefinition
                && entry.symbol.as_deref() == Some("alpha")
                && entry.path.as_deref() == Some("src/lib.rs"))
    );
    assert!(
        bundle
            .entries
            .iter()
            .any(|entry| entry.symbol.as_deref() == Some("fresh_helper")
                && entry.path.as_deref() == Some("src/fresh.rs")),
        "untracked working-tree content is part of the snapshot: {bundle:?}"
    );

    // symbol_lookup: exact definitions with metadata.
    let response = server.handle(
        "t-2",
        &request(
            "rev-tools",
            "quality",
            ReviewerCapability::SymbolLookup,
            serde_json::json!({"symbol": "alpha"}),
        ),
    );
    let bundle = bundle_of(&response);
    assert_eq!(bundle.entries.len(), 1);
    assert_eq!(bundle.entries[0].kind, ContextKind::SymbolDefinition);
    assert_eq!(bundle.entries[0].path.as_deref(), Some("src/lib.rs"));
    assert_eq!(bundle.entries[0].line_start, Some(1));
    assert_eq!(bundle.entries[0].line_end, Some(3));
    // Unknown symbols are successful empty answers, not errors.
    let response = server.handle(
        "t-3",
        &request(
            "rev-tools",
            "quality",
            ReviewerCapability::SymbolLookup,
            serde_json::json!({"symbol": "does_not_exist"}),
        ),
    );
    assert!(bundle_of(&response).is_empty());

    // relation_lookup: definitions plus references.
    let response = server.handle(
        "t-4",
        &request(
            "rev-tools",
            "quality",
            ReviewerCapability::RelationLookup,
            serde_json::json!({"symbol": "alpha"}),
        ),
    );
    let bundle = bundle_of(&response);
    assert!(
        bundle
            .entries
            .iter()
            .any(|entry| entry.kind == ContextKind::SymbolDefinition)
    );
    assert!(
        bundle
            .entries
            .iter()
            .any(|entry| entry.kind == ContextKind::SymbolReference
                && entry.path.as_deref() == Some("src/main.rs"))
    );

    // source_lookup: whole file and an exact line range.
    let response = server.handle(
        "t-5",
        &request(
            "rev-tools",
            "quality",
            ReviewerCapability::SourceLookup,
            serde_json::json!({"path": "src/lib.rs"}),
        ),
    );
    let bundle = bundle_of(&response);
    assert_eq!(bundle.entries.len(), 1);
    assert_eq!(bundle.entries[0].kind, ContextKind::Source);
    assert_eq!(bundle.entries[0].path.as_deref(), Some("src/lib.rs"));
    assert_eq!(
        (bundle.entries[0].line_start, bundle.entries[0].line_end),
        (Some(1), Some(3))
    );
    assert!(bundle.entries[0].text.contains("pub fn alpha() -> u8 {"));

    let response = server.handle(
        "t-6",
        &request(
            "rev-tools",
            "quality",
            ReviewerCapability::SourceLookup,
            serde_json::json!({"path": "src/lib.rs", "range": {"line_start": 2, "line_end": 3}}),
        ),
    );
    let bundle = bundle_of(&response);
    assert_eq!(bundle.entries[0].text, "    1\n}\n");
    assert_eq!(
        (bundle.entries[0].line_start, bundle.entries[0].line_end),
        (Some(2), Some(3))
    );

    // A range past the last line is a successful empty answer.
    let response = server.handle(
        "t-7",
        &request(
            "rev-tools",
            "quality",
            ReviewerCapability::SourceLookup,
            serde_json::json!({"path": "src/lib.rs", "range": {"line_start": 9, "line_end": 12}}),
        ),
    );
    assert!(bundle_of(&response).is_empty());

    // Every served call consumed exactly one lookup.
    assert_eq!(server.lookups_used("quality"), 7);
    assert!(server.context_bytes_used("quality") > 0);
}

#[test]
fn gates_reject_unknown_identities_denied_capabilities_and_bad_arguments() {
    let repo = Repo::new("tools-gates");
    write(
        &repo.root,
        "src/lib.rs",
        "pub fn alpha() -> u8 {\n    1\n}\n",
    );
    commit_all(&repo.root, "one");

    let setup = ReviewSetup::prepare(&repo.root, &SnapshotSpec::Working);
    let reviewers = [spec(
        "quality",
        &[ReviewerCapability::SymbolLookup],
        4,
        64 * 1024,
    )];
    let mut server = setup.server("rev-gates", &reviewers);

    // Wrong review.
    let response = server.handle(
        "t-1",
        &request(
            "rev-other",
            "quality",
            ReviewerCapability::SymbolLookup,
            serde_json::json!({"symbol": "alpha"}),
        ),
    );
    failure_of(&response, tools::tool_error::UNKNOWN_REVIEW);

    // Unknown reviewer.
    let response = server.handle(
        "t-2",
        &request(
            "rev-gates",
            "security",
            ReviewerCapability::SymbolLookup,
            serde_json::json!({"symbol": "alpha"}),
        ),
    );
    failure_of(&response, tools::tool_error::UNKNOWN_REVIEWER);

    // Capability not in the allowlist: denied even with valid arguments.
    let response = server.handle(
        "t-3",
        &request(
            "rev-gates",
            "quality",
            ReviewerCapability::SourceLookup,
            serde_json::json!({"path": "src/lib.rs"}),
        ),
    );
    let message = failure_of(&response, tools::tool_error::CAPABILITY_DENIED);
    assert!(message.contains("source_lookup"), "{message}");
    assert_eq!(
        server.lookups_used("quality"),
        0,
        "denied calls consume nothing"
    );

    // Malformed arguments never consume a lookup.
    for bad in [
        serde_json::json!({"symbol": ""}),
        serde_json::json!({"symbol": 42}),
        serde_json::json!({"name": "alpha"}),
        serde_json::json!({"symbol": "alpha", "extra": true}),
        serde_json::json!({}),
        serde_json::json!({"symbol": "x".repeat(tools::MAX_SYMBOL_BYTES + 1)}),
    ] {
        let response = server.handle(
            "t-4",
            &request(
                "rev-gates",
                "quality",
                ReviewerCapability::SymbolLookup,
                bad,
            ),
        );
        failure_of(&response, tools::tool_error::INVALID_ARGUMENTS);
    }

    // source_lookup-specific argument validation.
    let response = server.handle(
        "t-5",
        &request(
            "rev-gates",
            "quality",
            ReviewerCapability::SymbolLookup,
            serde_json::json!({"symbol": "alpha", "path": "src/lib.rs"}),
        ),
    );
    failure_of(&response, tools::tool_error::INVALID_ARGUMENTS);
    assert_eq!(server.lookups_used("quality"), 0);

    // A valid call still works after all the rejections.
    let response = server.handle(
        "t-6",
        &request(
            "rev-gates",
            "quality",
            ReviewerCapability::SymbolLookup,
            serde_json::json!({"symbol": "alpha"}),
        ),
    );
    assert_eq!(bundle_of(&response).entries.len(), 1);
    assert_eq!(server.lookups_used("quality"), 1);
}

#[test]
fn source_lookup_validates_paths_ranges_and_content() {
    let repo = Repo::new("tools-source");
    write(
        &repo.root,
        "src/lib.rs",
        "pub fn alpha() -> u8 {\n    1\n}\n",
    );
    std::fs::write(repo.root.join("blob.bin"), [0xff, 0xfe, 0x00, 0x01, 0x80]).unwrap();
    std::fs::write(repo.root.join("empty.txt"), b"").unwrap();
    commit_all(&repo.root, "one");

    let setup = ReviewSetup::prepare(&repo.root, &SnapshotSpec::Working);
    let reviewers = [spec(
        "quality",
        &[ReviewerCapability::SourceLookup],
        8,
        64 * 1024,
    )];
    let mut server = setup.server("rev-source", &reviewers);

    // Escaping and malformed paths are argument errors.
    for bad in ["../outside.txt", "/etc/passwd", ".git/config", "a//b.txt"] {
        let response = server.handle(
            "t-1",
            &request(
                "rev-source",
                "quality",
                ReviewerCapability::SourceLookup,
                serde_json::json!({"path": bad}),
            ),
        );
        failure_of(&response, tools::tool_error::INVALID_ARGUMENTS);
    }

    // Absent but well-formed paths are snapshot-membership failures.
    let response = server.handle(
        "t-2",
        &request(
            "rev-source",
            "quality",
            ReviewerCapability::SourceLookup,
            serde_json::json!({"path": "src/missing.rs"}),
        ),
    );
    let message = failure_of(&response, tools::tool_error::PATH_NOT_IN_SNAPSHOT);
    assert!(message.contains("src/missing.rs"), "{message}");

    // Invalid ranges are argument errors.
    for range in [
        serde_json::json!({"line_start": 0, "line_end": 1}),
        serde_json::json!({"line_start": 3, "line_end": 2}),
        serde_json::json!({"line_start": 1}),
    ] {
        let response = server.handle(
            "t-3",
            &request(
                "rev-source",
                "quality",
                ReviewerCapability::SourceLookup,
                serde_json::json!({"path": "src/lib.rs", "range": range}),
            ),
        );
        failure_of(&response, tools::tool_error::INVALID_ARGUMENTS);
    }

    // Binary content is refused rather than mangled.
    let response = server.handle(
        "t-4",
        &request(
            "rev-source",
            "quality",
            ReviewerCapability::SourceLookup,
            serde_json::json!({"path": "blob.bin"}),
        ),
    );
    failure_of(&response, tools::tool_error::BINARY_CONTENT);

    // Empty files read back as empty entries without line numbers.
    let response = server.handle(
        "t-5",
        &request(
            "rev-source",
            "quality",
            ReviewerCapability::SourceLookup,
            serde_json::json!({"path": "empty.txt"}),
        ),
    );
    let bundle = bundle_of(&response);
    assert_eq!(bundle.entries.len(), 1);
    assert_eq!(bundle.entries[0].text, "");
    assert_eq!(bundle.entries[0].line_start, None);

    // Invalid arguments (bad ranges) consumed nothing; the seven
    // dispatched lookups (4 escaping paths, 1 absent path, 1 binary,
    // 1 empty file) each consumed one call.
    assert_eq!(server.lookups_used("quality"), 7);
}

#[test]
fn lookup_count_is_enforced_per_reviewer() {
    let repo = Repo::new("tools-count");
    write(
        &repo.root,
        "src/lib.rs",
        "pub fn alpha() -> u8 {\n    1\n}\n",
    );
    commit_all(&repo.root, "one");

    let setup = ReviewSetup::prepare(&repo.root, &SnapshotSpec::Working);
    let reviewers = [
        spec("quality", &[ReviewerCapability::SymbolLookup], 2, 64 * 1024),
        spec(
            "security",
            &[ReviewerCapability::SymbolLookup],
            1,
            64 * 1024,
        ),
    ];
    let mut server = setup.server("rev-count", &reviewers);

    let call = |server: &mut ReviewerToolServer, reviewer: &str, symbol: &str| {
        server.handle(
            "t",
            &request(
                "rev-count",
                reviewer,
                ReviewerCapability::SymbolLookup,
                serde_json::json!({"symbol": symbol}),
            ),
        )
    };

    // quality: two calls allowed, the third is exhausted.
    assert_eq!(
        bundle_of(&call(&mut server, "quality", "alpha"))
            .entries
            .len(),
        1
    );
    assert_eq!(
        bundle_of(&call(&mut server, "quality", "alpha"))
            .entries
            .len(),
        1
    );
    let response = call(&mut server, "quality", "alpha");
    failure_of(&response, tools::tool_error::LOOKUP_BUDGET_EXHAUSTED);
    assert_eq!(server.lookups_used("quality"), 2);

    // Budgets are per reviewer: security still has its own call.
    assert_eq!(
        bundle_of(&call(&mut server, "security", "alpha"))
            .entries
            .len(),
        1
    );
    let response = call(&mut server, "security", "alpha");
    failure_of(&response, tools::tool_error::LOOKUP_BUDGET_EXHAUSTED);

    // A zero-lookup reviewer can never call.
    let reviewers = [spec(
        "performance",
        &[ReviewerCapability::SymbolLookup],
        0,
        64 * 1024,
    )];
    let mut server = setup.server("rev-count", &reviewers);
    let response = server.handle(
        "t",
        &request(
            "rev-count",
            "performance",
            ReviewerCapability::SymbolLookup,
            serde_json::json!({"symbol": "alpha"}),
        ),
    );
    failure_of(&response, tools::tool_error::LOOKUP_BUDGET_EXHAUSTED);
}

#[test]
fn returned_context_budget_is_enforced() {
    let repo = Repo::new("tools-bytes");
    // One file large enough that its raw source exceeds the test budgets.
    let mut source = String::new();
    for index in 0..200 {
        source.push_str(&format!(
            "pub fn filler_{index:03}() -> u8 {{\n    {index}\n}}\n"
        ));
    }
    write(&repo.root, "src/big.rs", &source);
    commit_all(&repo.root, "one");

    let setup = ReviewSetup::prepare(&repo.root, &SnapshotSpec::Working);

    // A budget smaller than the file serves a bounded prefix.
    let reviewers = [spec("quality", &[ReviewerCapability::SourceLookup], 8, 600)];
    let mut server = setup.server("rev-bytes", &reviewers);
    let response = server.handle(
        "t-1",
        &request(
            "rev-bytes",
            "quality",
            ReviewerCapability::SourceLookup,
            serde_json::json!({"path": "src/big.rs"}),
        ),
    );
    let bundle = bundle_of(&response);
    assert_eq!(bundle.entries.len(), 1);
    assert!(bundle.text_bytes <= 600, "bundle must fit the budget");
    assert!(bundle.truncated);
    assert!(bundle.entries[0].text.starts_with("pub fn filler_000"));
    assert_eq!(
        server.context_bytes_used("quality") as usize,
        bundle.text_bytes
    );

    // The budget is spent: the next non-empty result is a typed failure.
    let response = server.handle(
        "t-2",
        &request(
            "rev-bytes",
            "quality",
            ReviewerCapability::SourceLookup,
            serde_json::json!({"path": "src/big.rs"}),
        ),
    );
    failure_of(&response, tools::tool_error::CONTEXT_BUDGET_EXHAUSTED);

    // Empty results never fail on budget.
    let response = server.handle(
        "t-3",
        &request(
            "rev-bytes",
            "quality",
            ReviewerCapability::SourceLookup,
            serde_json::json!({"path": "src/big.rs", "range": {"line_start": 9999, "line_end": 9999}}),
        ),
    );
    assert!(bundle_of(&response).is_empty());

    // Multi-entry answers are trimmed weakest-first into the remaining
    // budget instead of failing.
    let reviewers = [spec(
        "quality",
        &[ReviewerCapability::ProjectLookup],
        8,
        4_000,
    )];
    let mut server = setup.server("rev-bytes", &reviewers);
    let response = server.handle(
        "t-4",
        &request(
            "rev-bytes",
            "quality",
            ReviewerCapability::ProjectLookup,
            serde_json::json!({"query": "filler_000 filler_001 filler_002 filler_003 filler_004 filler_005 filler_006 filler_007"}),
        ),
    );
    let first = bundle_of(&response);
    assert!(first.entries.len() >= 2, "several entries expected");
    assert!(first.text_bytes <= 4_000);
    let consumed = first.text_bytes;

    let response = server.handle(
        "t-5",
        &request(
            "rev-bytes",
            "quality",
            ReviewerCapability::ProjectLookup,
            serde_json::json!({"query": "filler_000 filler_001 filler_002 filler_003 filler_004 filler_005 filler_006 filler_007"}),
        ),
    );
    let second = bundle_of(&response);
    let remaining = 4_000usize - consumed;
    assert!(second.text_bytes <= remaining.max(1));
    assert!(
        second.entries.len() <= first.entries.len(),
        "the second answer cannot carry more entries than fit"
    );
    assert_eq!(
        server.context_bytes_used("quality") as usize,
        consumed + second.text_bytes
    );
    // The strongest entry is stable across calls: trimming is from the end.
    assert_eq!(second.entries[0], first.entries[0]);
}

#[test]
fn historical_commit_snapshots_never_serve_current_files() {
    let repo = Repo::new("tools-history");
    write(
        &repo.root,
        "src/lib.rs",
        "pub fn alpha() -> u8 {\n    1\n}\n",
    );
    let first = commit_all(&repo.root, "one");
    write(
        &repo.root,
        "src/lib.rs",
        "pub fn beta() -> u8 {\n    2\n}\n",
    );
    write(
        &repo.root,
        "src/extra.rs",
        "pub fn extra() -> u8 {\n    3\n}\n",
    );
    commit_all(&repo.root, "two");
    // The working tree now carries the "two" state.

    let setup = ReviewSetup::prepare(&repo.root, &SnapshotSpec::Commit { revision: first });
    let reviewers = [spec("quality", &ReviewerCapability::ALL, 8, 256 * 1024)];
    let mut server = setup.server("rev-history", &reviewers);

    // Symbols of the historical snapshot resolve; current ones do not.
    let response = server.handle(
        "t-1",
        &request(
            "rev-history",
            "quality",
            ReviewerCapability::SymbolLookup,
            serde_json::json!({"symbol": "alpha"}),
        ),
    );
    assert_eq!(bundle_of(&response).entries.len(), 1);
    let response = server.handle(
        "t-2",
        &request(
            "rev-history",
            "quality",
            ReviewerCapability::SymbolLookup,
            serde_json::json!({"symbol": "beta"}),
        ),
    );
    assert!(
        bundle_of(&response).is_empty(),
        "current symbols never leak"
    );

    // Source reads return the historical bytes, not the working tree.
    let response = server.handle(
        "t-3",
        &request(
            "rev-history",
            "quality",
            ReviewerCapability::SourceLookup,
            serde_json::json!({"path": "src/lib.rs"}),
        ),
    );
    let bundle = bundle_of(&response);
    assert!(bundle.entries[0].text.contains("pub fn alpha()"));
    assert!(!bundle.entries[0].text.contains("beta"));

    // A file that only exists in the current working tree is absent from
    // the historical snapshot even though it is on disk right now.
    let response = server.handle(
        "t-4",
        &request(
            "rev-history",
            "quality",
            ReviewerCapability::SourceLookup,
            serde_json::json!({"path": "src/extra.rs"}),
        ),
    );
    failure_of(&response, tools::tool_error::PATH_NOT_IN_SNAPSHOT);

    // project_lookup stays inside the historical snapshot too.
    let response = server.handle(
        "t-5",
        &request(
            "rev-history",
            "quality",
            ReviewerCapability::ProjectLookup,
            serde_json::json!({"query": "beta extra"}),
        ),
    );
    let bundle = bundle_of(&response);
    assert!(
        bundle
            .entries
            .iter()
            .all(|entry| entry.symbol.as_deref() != Some("beta")
                && entry.symbol.as_deref() != Some("extra")),
        "no current-tree symbols may appear: {bundle:?}"
    );
}

#[test]
fn linked_worktrees_with_equal_paths_stay_isolated() {
    let repo = Repo::new("tools-worktrees");
    write(
        &repo.root,
        "src/mod.rs",
        "pub fn alpha_fn() -> u8 {\n    1\n}\n",
    );
    commit_all(&repo.root, "alpha");
    git(&repo.root, &["checkout", "--quiet", "-b", "feature"]);
    write(
        &repo.root,
        "src/mod.rs",
        "pub fn beta_fn() -> u8 {\n    2\n}\n",
    );
    commit_all(&repo.root, "beta");
    git(&repo.root, &["checkout", "--quiet", &repo.branch]);

    // A linked worktree on the feature branch.
    let wt_path = repo.sibling("wt");
    git(
        &repo.root,
        &[
            "worktree",
            "add",
            "--quiet",
            wt_path.to_str().unwrap(),
            "feature",
        ],
    );
    let wt_root = wt_path.canonicalize().unwrap();

    // Both worktrees share one common dir (and therefore one shared
    // cache database).
    fn common_dir(root: &Path) -> PathBuf {
        let printed = git_out(root, &["rev-parse", "--git-common-dir"]);
        let path = PathBuf::from(&printed);
        if path.is_absolute() {
            path
        } else {
            root.join(path)
        }
        .canonicalize()
        .expect("common dir exists")
    }
    assert_eq!(common_dir(&repo.root), common_dir(&wt_root));

    let main_setup = ReviewSetup::prepare(&repo.root, &SnapshotSpec::Working);
    let wt_setup = ReviewSetup::prepare(&wt_root, &SnapshotSpec::Working);

    let reviewers = [spec("quality", &ReviewerCapability::ALL, 8, 256 * 1024)];
    let mut main_server = main_setup.server("rev-main", &reviewers);
    let mut wt_server = wt_setup.server("rev-wt", &reviewers);

    // Each worktree resolves its own symbols at the same path.
    let response = main_server.handle(
        "t-1",
        &request(
            "rev-main",
            "quality",
            ReviewerCapability::SymbolLookup,
            serde_json::json!({"symbol": "alpha_fn"}),
        ),
    );
    assert_eq!(bundle_of(&response).entries.len(), 1);
    let response = main_server.handle(
        "t-2",
        &request(
            "rev-main",
            "quality",
            ReviewerCapability::SymbolLookup,
            serde_json::json!({"symbol": "beta_fn"}),
        ),
    );
    assert!(bundle_of(&response).is_empty(), "no cross-worktree leakage");

    let response = wt_server.handle(
        "t-3",
        &request(
            "rev-wt",
            "quality",
            ReviewerCapability::SymbolLookup,
            serde_json::json!({"symbol": "beta_fn"}),
        ),
    );
    assert_eq!(bundle_of(&response).entries.len(), 1);
    let response = wt_server.handle(
        "t-4",
        &request(
            "rev-wt",
            "quality",
            ReviewerCapability::SymbolLookup,
            serde_json::json!({"symbol": "alpha_fn"}),
        ),
    );
    assert!(bundle_of(&response).is_empty(), "no cross-worktree leakage");

    // Each worktree reads its own bytes at the same path.
    let response = main_server.handle(
        "t-5",
        &request(
            "rev-main",
            "quality",
            ReviewerCapability::SourceLookup,
            serde_json::json!({"path": "src/mod.rs"}),
        ),
    );
    let bundle = bundle_of(&response);
    assert!(bundle.entries[0].text.contains("alpha_fn"));
    assert!(!bundle.entries[0].text.contains("beta_fn"));

    let response = wt_server.handle(
        "t-6",
        &request(
            "rev-wt",
            "quality",
            ReviewerCapability::SourceLookup,
            serde_json::json!({"path": "src/mod.rs"}),
        ),
    );
    let bundle = bundle_of(&response);
    assert!(bundle.entries[0].text.contains("beta_fn"));
    assert!(!bundle.entries[0].text.contains("alpha_fn"));
}

#[test]
fn fake_runtime_tool_calls_are_served_by_rust_over_the_protocol() {
    let repo = Repo::new("tools-e2e");
    write(
        &repo.root,
        "src/probe.rs",
        "pub fn runtime_probe() -> u8 {\n    1\n}\n",
    );
    commit_all(&repo.root, "one");

    let setup = ReviewSetup::prepare(&repo.root, &SnapshotSpec::Working);
    let reviewers = [spec(
        "quality",
        &[ReviewerCapability::ProjectLookup],
        4,
        64 * 1024,
    )];
    let mut server = setup.server("rev-e2e", &reviewers);

    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("runtime/pi-host/src/main.ts");
    let launch = PiHostLaunch::new("node", script, HostRuntime::Fake);
    let mut host = HostProcess::spawn(&launch).expect("spawn failed");
    host.wait_ready(Duration::from_secs(20))
        .expect("host_ready");

    let payload = StartReviewPayload {
        review_id: "rev-e2e".to_string(),
        kind: nisse::domain::review::ReviewKind::Code,
        reviewers: reviewers.to_vec(),
    };
    host.start_review(&payload).expect("start_review failed");
    let outcomes = host
        .complete_review(
            "rev-e2e",
            &["quality".to_string()],
            &mut |request: &ToolRequestPayload| server.handle("driven", request),
            Duration::from_secs(20),
        )
        .expect("review should complete");

    // The fake runtime performed exactly one project_lookup; Rust served
    // it against the exact snapshot and the summary reflects real entries.
    match &outcomes[0].1 {
        ReviewerOutcome::Completed(report) => {
            assert!(
                report.summary.contains("tools=1"),
                "summary: {}",
                report.summary
            );
            assert!(
                report.summary.contains("tool_entries=1"),
                "the lookup must return the snapshot's symbol: {}",
                report.summary
            );
            assert!(
                !report.summary.contains("tool_error"),
                "the lookup must succeed: {}",
                report.summary
            );
        }
        other => panic!("expected completion, got {other:?}"),
    }
    assert_eq!(server.lookups_used("quality"), 1);

    host.shutdown(Duration::from_secs(10)).expect("clean exit");
}
