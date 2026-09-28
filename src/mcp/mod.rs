//! MCP stdio adapter (Nisse plan sections 9, 61, 67; milestone 26).
//!
//! `nisse mcp` exposes the four frozen Nisse Core operations as MCP
//! tools over the stdio transport:
//!
//! | tool             | Nisse Core operation                            |
//! | :--------------- | :---------------------------------------------- |
//! | `review_plan`    | [`App::plan_review_operation`]                  |
//! | `review_code`    | [`App::code_review_operation`]                  |
//! | `query_project`  | [`App::query_operation`]                        |
//! | `record_feedback`| [`App::feedback_operation`]                     |
//!
//! The adapter is deliberately thin: every tool is one call into the
//! same in-process core operations the CLI uses (the Pi extension
//! reaches them through the CLI), so CLI, Pi, and MCP share identical
//! review, retrieval, and learning behavior (permanent invariant 25).
//! The adapter adds no business logic; it validates tool input shape
//! and maps results onto MCP tool results.
//!
//! Frozen adapter semantics:
//!
//! - **stdio transport only** (plan section 67): newline-delimited
//!   JSON-RPC on stdin/stdout. stdout carries protocol messages only;
//!   all logs go to stderr (`nisse::app::init_tracing`), so the
//!   protocol stream stays pure even while reviews spawn child
//!   processes.
//! - **noninteractive** (plan section 44): no tool ever prompts.
//!   Reviews without a contributor identity leave learning pending
//!   locally; feedback fails explicitly while retaining the outcome
//!   as pending state — exactly like the CLI.
//! - **serialized operations**: tool calls hold an async mutex while
//!   their blocking Nisse operation runs, so one server executes one
//!   operation at a time (conservative, deterministic local-state
//!   access). The server itself keeps serving concurrent requests in
//!   arrival order.
//! - **tool-level vs protocol errors**: structurally invalid input
//!   (blank or missing required fields) is rejected before any Nisse
//!   work runs as a JSON-RPC `invalid_params` error (malformed
//!   argument values that fail SDK deserialization surface as an
//!   `isError` tool result). An operation that ran and failed (for
//!   example a review with too few valid reviewer reports) is a
//!   successful response whose tool result carries `isError: true`
//!   with Nisse's typed error message, mirroring the CLI's failure
//!   text.

use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::schemars;
use rmcp::transport::stdio;
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};

use crate::app::App;
use crate::domain::records::FeedbackOutcome;
use crate::git::SnapshotSpec;
use crate::jury::review::PlanInput;

/// Run the MCP stdio server until the client disconnects (stdin EOF).
///
/// Builds a private multi-thread tokio runtime: the rest of Nisse is
/// synchronous blocking code, and only the MCP transport is async.
pub fn run_server() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| anyhow::anyhow!("failed to build the async runtime: {err}"))?;
    runtime.block_on(async {
        let (rx, tx): (tokio::io::Stdin, tokio::io::Stdout) = stdio();
        let service = match NisseMcpServer::new().serve((rx, tx)).await {
            Ok(service) => service,
            // A client that disconnects before completing `initialize`
            // (for example a probe, or an immediately closed pipe) is a
            // clean session end, not a server failure: nothing was
            // requested and nothing was done.
            Err(rmcp::service::ServerInitializeError::ConnectionClosed(_)) => return Ok(()),
            Err(err) => {
                return Err(anyhow::anyhow!(
                    "failed to start the MCP stdio server: {err}"
                ));
            }
        };
        service
            .waiting()
            .await
            .map_err(|err| anyhow::anyhow!("the MCP stdio server terminated: {err}"))?;
        Ok(())
    })
}

/// The MCP server state: a fresh [`App`] per operation (the
/// application root owns no state) behind a mutex that serializes
/// tool operations.
#[derive(Debug, Clone)]
struct NisseMcpServer {
    /// Serializes blocking Nisse operations (see the module docs).
    operation_lock: Arc<tokio::sync::Mutex<()>>,
}

#[tool_router]
impl NisseMcpServer {
    fn new() -> Self {
        NisseMcpServer {
            operation_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Serialize and run one blocking Nisse operation on the blocking
    /// pool. Panicking operations become internal protocol errors —
    /// Nisse operations are expected to return typed errors instead.
    async fn run_operation<T, F>(
        &self,
        operation: F,
    ) -> Result<Result<T, crate::app::AppError>, McpError>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T, crate::app::AppError> + Send + 'static,
    {
        let _guard = self.operation_lock.lock().await;
        tokio::task::spawn_blocking(operation)
            .await
            .map_err(|err| McpError::internal_error(format!("the operation failed: {err}"), None))
    }

    /// Run one Nisse operation and render its outcome as a tool
    /// result: success becomes strict JSON text content, a typed
    /// Nisse failure becomes an `isError` tool result whose message
    /// matches the CLI's failure text (the operations already carry
    /// their own contextual prefix).
    async fn run_json_operation<T, F>(
        &self,
        failure: &'static str,
        operation: F,
    ) -> Result<CallToolResult, McpError>
    where
        T: Serialize + Send + 'static,
        F: FnOnce() -> Result<T, crate::app::AppError> + Send + 'static,
    {
        match self.run_operation(operation).await? {
            Ok(value) => {
                let json = serde_json::to_string(&value).map_err(|err| {
                    McpError::internal_error(
                        format!("failed to serialize the {failure} result: {err}"),
                        None,
                    )
                })?;
                Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
            }
            Err(err) => Ok(tool_error(err.to_string())),
        }
    }

    /// Run one Nisse operation that yields already-rendered strict
    /// JSON text (the shared report renderer) and return it verbatim
    /// as the tool result content.
    async fn run_text_operation<F>(&self, operation: F) -> Result<CallToolResult, McpError>
    where
        F: FnOnce() -> Result<String, crate::app::AppError> + Send + 'static,
    {
        match self.run_operation(operation).await? {
            Ok(json) => Ok(CallToolResult::success(vec![ContentBlock::text(json)])),
            Err(err) => Ok(tool_error(err.to_string())),
        }
    }

    #[tool(
        description = "Run a Nisse reviewer-jury review of an implementation plan before implementing it. Pass the complete plan text; it is reviewed against the current working-tree snapshot of the server's working directory. Returns Nisse ReviewResult JSON: review_id, summary, findings[] with id, severity (blocker|high|medium|low|info), impact_class, title, claim, explanation, recommendation, evidence, reviewer_ids, cross_validated. Address valid blocker/high findings before implementing; submit known outcomes with record_feedback."
    )]
    async fn review_plan(
        &self,
        Parameters(params): Parameters<ReviewPlanParams>,
    ) -> Result<CallToolResult, McpError> {
        reject_blank(&params.plan, "plan")?;
        // The plan travels as tool input, not as a worktree file, so it
        // carries no repository-relative path (like `review plan --stdin`).
        // The text passes through unmodified; the shared operation owns
        // the plan bounds (empty/size checks).
        let plan = params.plan;
        self.run_json_operation("review plan", move || {
            App::new().plan_review_operation(PlanInput {
                text: plan,
                path: None,
            })
        })
        .await
    }

    #[tool(
        description = "Run a Nisse reviewer-jury review of code changes after implementing and passing project tests. Default scope `working` compares HEAD against the final working-tree state (staged + unstaged + untracked non-ignored); `staged` compares HEAD against the index; `commit` compares one commit against its first parent (requires `commit`); `range` compares `<base>..<head>` trees (requires `range`); `branch` compares merge-base(baseRef, HEAD) against HEAD (requires `baseRef`). Returns ReviewResult JSON like review_plan; use review_id and findings[].id for feedback."
    )]
    async fn review_code(
        &self,
        Parameters(params): Parameters<ReviewCodeParams>,
    ) -> Result<CallToolResult, McpError> {
        let spec = snapshot_spec(&params)?;
        self.run_json_operation("review code", move || {
            App::new().code_review_operation(spec)
        })
        .await
    }

    #[tool(
        description = "Run a Nisse reviewer-jury review of investigation results (PLAN_INVESTIGATION.md): claim review (supported / partially supported / contradicted / insufficiently supported / not applicable / outside scope with confidence tiers and overstated notes) plus coverage review against the task. Task and result accept literal text, an explicit @file reference, or a path naming an existing file. Returns InvestigationResult JSON."
    )]
    async fn review_investigation(
        &self,
        Parameters(params): Parameters<ReviewInvestigationParams>,
    ) -> Result<CallToolResult, McpError> {
        reject_blank(&params.task, "task")?;
        reject_blank(&params.result, "result")?;
        self.run_json_operation("review investigation", move || {
            App::new().investigation_operation(params.task, params.result)
        })
        .await
    }

    #[tool(
        description = "Query Nisse's deterministic project index for the current working-tree snapshot of the server's working directory: source symbols and relations, project facts, lessons, and built-in practices. Runs no LLM review; useful for investigating the project and checking finding evidence. Returns a JSON context bundle (query, snapshot, truncated, entries[])."
    )]
    async fn query_project(
        &self,
        Parameters(params): Parameters<QueryProjectParams>,
    ) -> Result<CallToolResult, McpError> {
        reject_blank(&params.query, "query")?;
        // Same strict JSON report as `nisse query --json`, verbatim; the
        // query text passes through unmodified.
        let query = params.query;
        self.run_text_operation(move || {
            App::new().query_operation(query).and_then(|outcome| {
                crate::retrieval::render_json(&outcome.query, &outcome.snapshot, &outcome.bundle)
                    .map_err(|err| {
                        crate::app::AppError::Other(anyhow::anyhow!("query failed: {err}"))
                    })
            })
        })
        .await
    }

    #[tool(
        description = "Record a known outcome for one Nisse finding: fixed (the implementation changed specifically to address it), accepted (valid but intentionally not fixed now), or dismissed (investigated and judged incorrect or not applicable). Submit only outcomes you actually know; submit nothing when unclear. Repeated submissions are idempotent. Creates immutable durable observations in .nisse/ (plan sections 49–50)."
    )]
    async fn record_feedback(
        &self,
        Parameters(params): Parameters<RecordFeedbackParams>,
    ) -> Result<CallToolResult, McpError> {
        reject_blank(&params.review_id, "reviewId")?;
        reject_blank(&params.finding_id, "findingId")?;
        let review_id = params.review_id;
        let finding_id = params.finding_id;
        let outcome = params.outcome.into_domain();
        self.run_json_operation("feedback", move || {
            App::new()
                .feedback_operation(&review_id, &finding_id, outcome)
                .map(|recording| FeedbackReport::new(review_id, finding_id, outcome, &recording))
        })
        .await
    }
}

#[tool_handler]
impl ServerHandler for NisseMcpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Nisse reviews plans and code changes with a deterministic reviewer jury. \
                 Review plan/code run LLM reviewer sessions and may take minutes; \
                 query_project is fast and deterministic. Record known finding \
                 outcomes with record_feedback.",
            )
    }
}

/// Input of `review_plan`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReviewPlanParams {
    /// Complete text of the implementation plan to review.
    #[schemars(description = "Complete text of the implementation plan to review.")]
    pub plan: String,
}

/// Input of `review_investigation` (PLAN_INVESTIGATION.md).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReviewInvestigationParams {
    /// The effective task the investigation was supposed to answer:
    /// literal text, an explicit `@file` reference, or a value naming an
    /// existing file.
    #[schemars(
        description = "The effective task (literal text, @file, or an existing file path)."
    )]
    pub task: String,
    /// The investigation output to review: literal text, an explicit
    /// `@file` reference, or a value naming an existing file.
    #[schemars(
        description = "The investigation output to review (literal text, @file, or an existing file path)."
    )]
    pub result: String,
}

/// Input of `review_code`: one scope plus its companion revision, when
/// the scope needs one.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReviewCodeParams {
    /// Which changes to review.
    #[schemars(
        description = "Which changes to review: working, staged, commit, range, or branch."
    )]
    pub scope: ReviewScope,
    /// Commit-ish to review (required when scope is `commit`).
    #[schemars(description = "Commit-ish to review (required when scope is `commit`).")]
    pub commit: Option<String>,
    /// Range `<base>..<head>` (required when scope is `range`).
    #[schemars(description = "Range `<base>..<head>` (required when scope is `range`).")]
    pub range: Option<String>,
    /// Base ref for the merge-base comparison (required when scope is
    /// `branch`).
    #[schemars(
        description = "Base ref for the merge-base comparison (required when scope is `branch`)."
    )]
    pub base_ref: Option<String>,
}

/// The frozen code-review scopes (plan section 60), matching the CLI
/// flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewScope {
    /// HEAD against the final working-tree state.
    Working,
    /// HEAD against the index.
    Staged,
    /// One commit against its first parent.
    Commit,
    /// `<base>..<head>` trees.
    Range,
    /// merge-base(base_ref, HEAD) against HEAD.
    Branch,
}

/// Input of `query_project`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct QueryProjectParams {
    /// Free-text project query.
    #[schemars(
        description = "Free-text project query: a symbol name, topic, path, or question about the indexed project."
    )]
    pub query: String,
}

/// Input of `record_feedback`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RecordFeedbackParams {
    /// Review identifier from the ReviewResult.
    #[schemars(description = "Review identifier from the ReviewResult (`review_id`).")]
    pub review_id: String,
    /// Finding identifier from the ReviewResult.
    #[schemars(description = "Finding identifier from the ReviewResult (`findings[].id`).")]
    pub finding_id: String,
    /// The known outcome.
    #[schemars(
        description = "fixed: implementation changed to address the finding. accepted: valid but intentionally not fixed now. dismissed: investigated and judged incorrect or not applicable."
    )]
    pub outcome: FeedbackOutcomeParam,
}

/// The frozen feedback outcomes (plan section 49), matching the CLI
/// flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackOutcomeParam {
    /// Implementation changed specifically to address the finding.
    Fixed,
    /// Valid but intentionally not fixed now.
    Accepted,
    /// Investigated and judged incorrect or not applicable.
    Dismissed,
}

impl FeedbackOutcomeParam {
    /// The frozen domain outcome (plan section 49).
    fn into_domain(self) -> FeedbackOutcome {
        match self {
            FeedbackOutcomeParam::Fixed => FeedbackOutcome::Fixed,
            FeedbackOutcomeParam::Accepted => FeedbackOutcome::Accepted,
            FeedbackOutcomeParam::Dismissed => FeedbackOutcome::Dismissed,
        }
    }
}

/// Machine-readable result of `record_feedback`: the submission state
/// plus the durable records it created.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackReport {
    pub review_id: String,
    pub finding_id: String,
    pub outcome: String,
    /// `recorded` or `already-recorded` (idempotent resubmission).
    pub status: &'static str,
    /// One entry per contributing reviewer, in finding reviewer order.
    pub observations: Vec<FeedbackObservation>,
    /// Routing lessons the deterministic maintenance pass created.
    pub lessons: Vec<FeedbackLesson>,
    /// Observations deferred by the directory-consent gate (0 unless
    /// status is `deferred-directory-consent`).
    #[serde(skip_serializing_if = "is_zero")]
    pub deferred: usize,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

/// One durable observation created by a feedback submission.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackObservation {
    pub reviewer_id: String,
    pub record_id: String,
}

/// One lesson created by the deterministic maintenance pass.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackLesson {
    pub reviewer_id: String,
    pub category: String,
    pub record_id: String,
    pub supersedes: usize,
}

impl FeedbackReport {
    /// Build the report from the operation's recording.
    fn new(
        review_id: String,
        finding_id: String,
        outcome: FeedbackOutcome,
        recording: &crate::learning::feedback::FeedbackRecording,
    ) -> Self {
        match recording {
            crate::learning::feedback::FeedbackRecording::Recorded {
                observations,
                lessons,
            } => FeedbackReport {
                review_id,
                finding_id,
                outcome: outcome.as_str().to_string(),
                status: "recorded",
                observations: observations
                    .iter()
                    .map(|(reviewer_id, record_id)| FeedbackObservation {
                        reviewer_id: reviewer_id.clone(),
                        record_id: record_id.clone(),
                    })
                    .collect(),
                lessons: lessons
                    .iter()
                    .map(|lesson| FeedbackLesson {
                        reviewer_id: lesson.reviewer_id.clone(),
                        category: lesson.category.clone(),
                        record_id: lesson.record_id.clone(),
                        supersedes: lesson.superseded.len(),
                    })
                    .collect(),
                deferred: 0,
            },
            crate::learning::feedback::FeedbackRecording::DeferredConsent { count } => {
                FeedbackReport {
                    review_id,
                    finding_id,
                    outcome: outcome.as_str().to_string(),
                    status: "deferred-directory-consent",
                    observations: Vec::new(),
                    lessons: Vec::new(),
                    deferred: *count,
                }
            }
            crate::learning::feedback::FeedbackRecording::AlreadyRecorded(_) => FeedbackReport {
                review_id,
                finding_id,
                outcome: outcome.as_str().to_string(),
                status: "already-recorded",
                observations: Vec::new(),
                lessons: Vec::new(),
                deferred: 0,
            },
        }
    }
}

/// Map validated `review_code` input to its snapshot specification
/// (plan section 60). A scope whose companion revision is missing or
/// blank is structurally invalid input.
fn snapshot_spec(params: &ReviewCodeParams) -> Result<SnapshotSpec, McpError> {
    match params.scope {
        ReviewScope::Working => Ok(SnapshotSpec::Working),
        ReviewScope::Staged => Ok(SnapshotSpec::Staged),
        ReviewScope::Commit => Ok(SnapshotSpec::Commit {
            revision: required_option_text(params.commit.clone(), "commit")?,
        }),
        ReviewScope::Range => Ok(SnapshotSpec::Range {
            spec: required_option_text(params.range.clone(), "range")?,
        }),
        ReviewScope::Branch => Ok(SnapshotSpec::Branch {
            base_ref: required_option_text(params.base_ref.clone(), "baseRef")?,
        }),
    }
}

/// Require a present, non-blank optional string parameter and pass it
/// through unmodified; a missing or blank value is structurally
/// invalid input. (Values are not trimmed: the core operations receive
/// exactly what the client sent, like CLI arguments.)
fn required_option_text(value: Option<String>, field: &str) -> Result<String, McpError> {
    match value {
        Some(value) => {
            reject_blank(&value, field)?;
            Ok(value)
        }
        None => Err(McpError::invalid_params(
            format!("`{field}` is required for this scope"),
            None,
        )),
    }
}

/// Reject blank string parameters (whitespace-only input is
/// structurally invalid); blank input is rejected before any Nisse
/// work runs.
fn reject_blank(value: &str, field: &str) -> Result<(), McpError> {
    if value.trim().is_empty() {
        return Err(McpError::invalid_params(
            format!("`{field}` must be a non-empty string"),
            None,
        ));
    }
    Ok(())
}

/// A tool-level error result: the operation ran and failed.
fn tool_error(message: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(format!("nisse error: {message}"))])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_parameters_are_rejected_before_any_work() {
        assert!(reject_blank("", "plan").is_err());
        assert!(reject_blank("   \n", "plan").is_err());
        assert!(reject_blank("  queue race  ", "plan").is_ok());

        // Present values pass through unmodified; absent ones are
        // invalid.
        assert_eq!(
            required_option_text(Some(" abc ".to_string()), "commit").unwrap(),
            " abc "
        );
        assert!(required_option_text(Some("  ".to_string()), "commit").is_err());
        assert!(required_option_text(None, "commit").is_err());
    }

    #[test]
    fn feedback_outcomes_map_to_the_frozen_domain_outcomes() {
        assert_eq!(
            FeedbackOutcomeParam::Fixed.into_domain(),
            FeedbackOutcome::Fixed
        );
        assert_eq!(
            FeedbackOutcomeParam::Accepted.into_domain(),
            FeedbackOutcome::Accepted
        );
        assert_eq!(
            FeedbackOutcomeParam::Dismissed.into_domain(),
            FeedbackOutcome::Dismissed
        );
    }

    #[test]
    fn scope_params_map_to_the_frozen_snapshot_specs() {
        let base = ReviewCodeParams {
            scope: ReviewScope::Working,
            commit: None,
            range: None,
            base_ref: None,
        };
        assert!(matches!(
            snapshot_spec(&base).unwrap(),
            SnapshotSpec::Working
        ));

        let mut params = base;
        params.scope = ReviewScope::Staged;
        assert!(matches!(
            snapshot_spec(&params).unwrap(),
            SnapshotSpec::Staged
        ));

        // A scope needing a revision without its companion field is
        // structurally invalid; blank values are too.
        params.scope = ReviewScope::Commit;
        params.commit = None;
        assert!(snapshot_spec(&params).is_err());
        params.commit = Some("   ".to_string());
        assert!(snapshot_spec(&params).is_err());
        params.commit = Some("abc123".to_string());
        assert!(matches!(
            snapshot_spec(&params).unwrap(),
            SnapshotSpec::Commit { .. }
        ));

        params.scope = ReviewScope::Range;
        assert!(snapshot_spec(&params).is_err());
        params.range = Some("v1..v2".to_string());
        assert!(matches!(
            snapshot_spec(&params).unwrap(),
            SnapshotSpec::Range { .. }
        ));

        params.scope = ReviewScope::Branch;
        assert!(snapshot_spec(&params).is_err());
        params.base_ref = Some("main".to_string());
        assert!(matches!(
            snapshot_spec(&params).unwrap(),
            SnapshotSpec::Branch { .. }
        ));
    }

    #[test]
    fn tool_input_schemas_parse_the_frozen_scope_and_outcome_values() {
        // The JSON schema values are frozen snake_case names (matching
        // the CLI flags); the deserializers accept exactly those.
        let scope: ReviewScope = serde_json::from_str("\"working\"").unwrap();
        assert_eq!(scope, ReviewScope::Working);
        for name in ["staged", "commit", "range", "branch"] {
            let json = format!("\"{name}\"");
            assert!(serde_json::from_str::<ReviewScope>(&json).is_ok());
        }
        assert!(serde_json::from_str::<ReviewScope>("\"nope\"").is_err());

        for name in ["fixed", "accepted", "dismissed"] {
            let json = format!("\"{name}\"");
            assert!(serde_json::from_str::<FeedbackOutcomeParam>(&json).is_ok());
        }
        assert!(serde_json::from_str::<FeedbackOutcomeParam>("\"maybe\"").is_err());

        // camelCase param names match the Pi extension tool inputs.
        let params: RecordFeedbackParams = serde_json::from_str(
            "{\"reviewId\":\"r1\",\"findingId\":\"f1\",\"outcome\":\"fixed\"}",
        )
        .unwrap();
        assert_eq!(params.review_id, "r1");
        assert_eq!(params.finding_id, "f1");
        assert_eq!(params.outcome, FeedbackOutcomeParam::Fixed);
    }

    #[test]
    fn deferred_consent_report_serializes_status_and_count() {
        let report = FeedbackReport::new(
            "rev-1".to_string(),
            "find-1".to_string(),
            FeedbackOutcome::Fixed,
            &crate::learning::feedback::FeedbackRecording::DeferredConsent { count: 2 },
        );
        assert_eq!(report.status, "deferred-directory-consent");
        assert_eq!(report.deferred, 2);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["status"], "deferred-directory-consent");
        assert_eq!(json["deferred"], 2);
    }

    #[test]
    fn recorded_report_omits_the_zero_deferred_field() {
        let report = FeedbackReport::new(
            "rev-1".to_string(),
            "find-1".to_string(),
            FeedbackOutcome::Fixed,
            &crate::learning::feedback::FeedbackRecording::AlreadyRecorded(FeedbackOutcome::Fixed),
        );
        let json = serde_json::to_value(&report).unwrap();
        assert!(json.get("deferred").is_none(), "{json}");
    }
}
