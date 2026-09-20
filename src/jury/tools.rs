//! Rust-side serving of the read-only reviewer tools (plan section 22).
//!
//! Reviewer sessions investigate through exactly four tools:
//! `project_lookup`, `symbol_lookup`, `source_lookup`, and
//! `relation_lookup`. The Pi review host bridges every tool call into a
//! `tool_request` protocol message; this module is the Rust side that
//! answers it.
//!
//! [`ReviewerToolServer`] owns the deterministic policy of every served
//! lookup:
//!
//! - **capability allowlist** — a request is only served when its tool is
//!   part of the requesting reviewer's `allowed_capabilities`;
//! - **lookup count** — every dispatched lookup consumes one of the
//!   reviewer's `max_lookup_calls`; exhausted reviewers get
//!   `lookup_budget_exhausted` errors instead of results;
//! - **returned context budget** — the text bytes of every served bundle
//!   count against the reviewer's `max_returned_context_bytes`; bundles
//!   are trimmed (never grown) to fit, and a reviewer whose budget is
//!   spent gets `context_budget_exhausted`;
//! - **snapshot correctness** — source reads go through
//!   [`SnapshotReader`](crate::git::SnapshotReader) (target side of the
//!   exact review snapshot) and symbol/relation/project lookups go
//!   through [`Retriever`](crate::retrieval::Retriever), which filters
//!   every entry through the worktree manifest. Cache presence never
//!   proves membership (plan section 26): a path that is not part of the
//!   target snapshot reads as absent.
//!
//! The server is created per review from the jury's execution specs. The
//! host cannot influence it beyond well-formed `tool_request` messages;
//! every argument is re-validated on this side of the boundary.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::protocol::{ToolRequestPayload, ToolResponsePayload};
use crate::domain::context::{ContextBundle, ContextEntry, ContextKind};
use crate::domain::reviewer::{ReviewerCapability, ReviewerExecutionSpec};
use crate::git::{GitError, SnapshotReader};
use crate::retrieval::Retriever;
use crate::retrieval::query::{ContextBudget, RetrievalQuery, assemble_bundle, truncate_text};

/// Stable error codes of `tool_response` failures served by Rust.
pub mod tool_error {
    /// The request names a review this server does not serve.
    pub const UNKNOWN_REVIEW: &str = "unknown_review";
    /// The request names a reviewer that is not part of this review.
    pub const UNKNOWN_REVIEWER: &str = "unknown_reviewer";
    /// The tool is not in the reviewer's `allowed_capabilities`.
    pub const CAPABILITY_DENIED: &str = "capability_denied";
    /// The reviewer consumed all of its `max_lookup_calls`.
    pub const LOOKUP_BUDGET_EXHAUSTED: &str = "lookup_budget_exhausted";
    /// The reviewer consumed all of its `max_returned_context_bytes`.
    pub const CONTEXT_BUDGET_EXHAUSTED: &str = "context_budget_exhausted";
    /// The tool arguments do not match the frozen per-tool schema.
    pub const INVALID_ARGUMENTS: &str = "invalid_arguments";
    /// `source_lookup` of a path that is not part of the target snapshot.
    pub const PATH_NOT_IN_SNAPSHOT: &str = "path_not_in_snapshot";
    /// `source_lookup` of content that is not valid UTF-8 text.
    pub const BINARY_CONTENT: &str = "binary_content";
    /// The retrieval layer failed (storage or query error).
    pub const RETRIEVAL_FAILED: &str = "retrieval_failed";
    /// A Git operation of the snapshot reader failed.
    pub const GIT_FAILED: &str = "git_failed";
}

/// Maximum accepted `symbol` argument size in bytes.
pub const MAX_SYMBOL_BYTES: usize = 512;

/// Maximum accepted `path` argument size in bytes.
pub const MAX_PATH_BYTES: usize = 4096;

/// Score of a raw `source_lookup` entry: the reviewer asked for exactly
/// this content, so it outranks every pre-selected signal.
const WEIGHT_SOURCE: f64 = 1.0;

/// `project_lookup(query)` arguments (plan section 22).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectLookupArgs {
    query: String,
}

impl ProjectLookupArgs {
    /// Validate the argument shape before any lookup is consumed.
    fn validate(&self) -> Result<(), ToolFailure> {
        if self.query.is_empty() {
            return Err(ToolFailure::invalid_arguments("`query` must be non-empty"));
        }
        if self.query.len() > crate::retrieval::query::MAX_QUERY_BYTES {
            return Err(ToolFailure::invalid_arguments(format!(
                "`query` exceeds {} bytes",
                crate::retrieval::query::MAX_QUERY_BYTES
            )));
        }
        Ok(())
    }
}

/// `symbol_lookup(symbol)` / `relation_lookup(symbol)` arguments.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SymbolArgs {
    symbol: String,
}

impl SymbolArgs {
    /// Validate the argument shape before any lookup is consumed.
    fn validate(&self) -> Result<(), ToolFailure> {
        if self.symbol.is_empty() {
            return Err(ToolFailure::invalid_arguments("`symbol` must be non-empty"));
        }
        if self.symbol.len() > MAX_SYMBOL_BYTES {
            return Err(ToolFailure::invalid_arguments(format!(
                "`symbol` exceeds {MAX_SYMBOL_BYTES} bytes"
            )));
        }
        Ok(())
    }
}

/// `source_lookup(path, optional_range)` arguments.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceLookupArgs {
    path: String,
    #[serde(default)]
    range: Option<SourceRange>,
}

impl SourceLookupArgs {
    /// Validate the argument shape before any lookup is consumed.
    fn validate(&self) -> Result<(), ToolFailure> {
        if self.path.is_empty() {
            return Err(ToolFailure::invalid_arguments("`path` must be non-empty"));
        }
        if self.path.len() > MAX_PATH_BYTES {
            return Err(ToolFailure::invalid_arguments(format!(
                "`path` exceeds {MAX_PATH_BYTES} bytes"
            )));
        }
        if let Some(range) = self.range {
            range.validate()?;
        }
        Ok(())
    }
}

/// Inclusive 1-based line range of a `source_lookup`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceRange {
    line_start: u32,
    line_end: u32,
}

impl SourceRange {
    /// Validate the range shape: 1-based, start before or at end.
    fn validate(self) -> Result<(), ToolFailure> {
        if self.line_start < 1 || self.line_end < self.line_start {
            return Err(ToolFailure::invalid_arguments(format!(
                "`range` must provide 1-based lines with line_start <= line_end, got {}..{}",
                self.line_start, self.line_end
            )));
        }
        Ok(())
    }
}

/// One typed tool-serving failure with a stable protocol error code.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolFailure {
    code: &'static str,
    message: String,
}

impl ToolFailure {
    fn invalid_arguments(message: impl Into<String>) -> Self {
        ToolFailure {
            code: tool_error::INVALID_ARGUMENTS,
            message: message.into(),
        }
    }
}

/// Per-reviewer enforcement state.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ReviewerBudget {
    lookups_used: u32,
    context_bytes_used: u64,
}

/// Serves the read-only reviewer tools of one review.
///
/// Constructed from the review's execution specs plus the snapshot-bound
/// retrieval engine and source reader. Budgets are enforced per reviewer
/// for the lifetime of the server (both rounds of the frozen retry policy
/// share one budget: the retry reuses the identical spec, and Rust cannot
/// distinguish host rounds, so the conservative shared budget applies).
pub struct ReviewerToolServer<'a> {
    review_id: String,
    specs: BTreeMap<String, ReviewerExecutionSpec>,
    budgets: BTreeMap<String, ReviewerBudget>,
    retriever: Retriever,
    reader: SnapshotReader<'a>,
}

impl<'a> ReviewerToolServer<'a> {
    /// Build the server for one review.
    pub fn new(
        review_id: impl Into<String>,
        reviewers: &[ReviewerExecutionSpec],
        retriever: Retriever,
        reader: SnapshotReader<'a>,
    ) -> Self {
        ReviewerToolServer {
            review_id: review_id.into(),
            specs: reviewers
                .iter()
                .map(|spec| (spec.reviewer_id.clone(), spec.clone()))
                .collect(),
            budgets: BTreeMap::new(),
            retriever,
            reader,
        }
    }

    /// The review this server serves.
    pub fn review_id(&self) -> &str {
        &self.review_id
    }

    /// Lookup calls consumed by `reviewer` so far.
    pub fn lookups_used(&self, reviewer: &str) -> u32 {
        self.budgets
            .get(reviewer)
            .map_or(0, |budget| budget.lookups_used)
    }

    /// Tool-returned context bytes consumed by `reviewer` so far.
    pub fn context_bytes_used(&self, reviewer: &str) -> u64 {
        self.budgets
            .get(reviewer)
            .map_or(0, |budget| budget.context_bytes_used)
    }

    /// Answer one `tool_request` with a complete `tool_response` payload.
    ///
    /// `request_id` must be the envelope id of the answered request;
    /// [`HostProcess::complete_review`](super::host::HostProcess::complete_review)
    /// stamps it authoritatively when driving the exchange.
    pub fn handle(
        &mut self,
        request_id: &str,
        request: &ToolRequestPayload,
    ) -> ToolResponsePayload {
        match self.dispatch(request) {
            Ok(bundle) => ToolResponsePayload::success(request_id, bundle),
            Err(failure) => {
                ToolResponsePayload::failure(request_id, failure.code, &failure.message)
            }
        }
    }

    /// Serve one request, enforcing every frozen bound in order:
    ///
    /// 1. the request belongs to this review;
    /// 2. the reviewer is part of the review;
    /// 3. the tool is in the reviewer's capability allowlist;
    /// 4. the reviewer still has lookup calls left;
    /// 5. the arguments match the frozen per-tool schema (a request with
    ///    invalid arguments consumes nothing);
    /// 6. the lookup executes against the exact target snapshot and
    ///    consumes one call;
    /// 7. the returned bundle is trimmed into the reviewer's remaining
    ///    context-byte budget.
    fn dispatch(&mut self, request: &ToolRequestPayload) -> Result<ContextBundle, ToolFailure> {
        if request.review_id != self.review_id {
            return Err(ToolFailure {
                code: tool_error::UNKNOWN_REVIEW,
                message: format!(
                    "request names review `{}` but this server serves `{}`",
                    request.review_id, self.review_id
                ),
            });
        }
        let Some(spec) = self.specs.get(&request.reviewer_id) else {
            return Err(ToolFailure {
                code: tool_error::UNKNOWN_REVIEWER,
                message: format!(
                    "request names reviewer `{}` which is not part of review `{}`",
                    request.reviewer_id, self.review_id
                ),
            });
        };
        if !spec.allowed_capabilities.contains(&request.tool) {
            return Err(ToolFailure {
                code: tool_error::CAPABILITY_DENIED,
                message: format!(
                    "reviewer `{}` was not granted the `{}` capability",
                    request.reviewer_id,
                    request.tool.name()
                ),
            });
        }
        // Copy the bounds out so no borrow of `self` outlives the gate.
        let (max_lookup_calls, max_context_bytes) =
            (spec.max_lookup_calls, spec.max_returned_context_bytes);
        let used = self
            .budgets
            .get(&request.reviewer_id)
            .copied()
            .unwrap_or_default();
        if used.lookups_used >= max_lookup_calls {
            return Err(ToolFailure {
                code: tool_error::LOOKUP_BUDGET_EXHAUSTED,
                message: format!(
                    "reviewer `{}` used all of its {max_lookup_calls} permitted lookup calls",
                    request.reviewer_id
                ),
            });
        }

        // Parse and validate the arguments before consuming anything: a
        // request whose arguments fail the frozen schema performed no
        // lookup. Every dispatched lookup (valid arguments) consumes one
        // call regardless of its outcome.
        let bundle = match request.tool {
            ReviewerCapability::ProjectLookup => {
                let args = parse_args::<ProjectLookupArgs>(&request.arguments)?;
                args.validate()?;
                self.consume_lookup(&request.reviewer_id, used);
                self.project_lookup(&args.query)?
            }
            ReviewerCapability::SymbolLookup => {
                let args = parse_args::<SymbolArgs>(&request.arguments)?;
                args.validate()?;
                self.consume_lookup(&request.reviewer_id, used);
                self.symbol_lookup(&args.symbol, false)?
            }
            ReviewerCapability::RelationLookup => {
                let args = parse_args::<SymbolArgs>(&request.arguments)?;
                args.validate()?;
                self.consume_lookup(&request.reviewer_id, used);
                self.symbol_lookup(&args.symbol, true)?
            }
            ReviewerCapability::SourceLookup => {
                let args = parse_args::<SourceLookupArgs>(&request.arguments)?;
                args.validate()?;
                self.consume_lookup(&request.reviewer_id, used);
                self.source_lookup(&args.path, args.range)?
            }
        };

        // Enforce the returned-context budget of this reviewer.
        let remaining = max_context_bytes.saturating_sub(used.context_bytes_used);
        let bundle = fit_bundle(bundle, remaining)?;
        let budget = self
            .budgets
            .get_mut(&request.reviewer_id)
            .expect("consume_lookup created the budget entry");
        budget.context_bytes_used += bundle.text_bytes as u64;
        Ok(bundle)
    }

    /// Record that one lookup of `reviewer` was dispatched (the budget
    /// entry is created on first use).
    fn consume_lookup(&mut self, reviewer: &str, used: ReviewerBudget) {
        self.budgets.insert(
            reviewer.to_owned(),
            ReviewerBudget {
                lookups_used: used.lookups_used + 1,
                context_bytes_used: used.context_bytes_used,
            },
        );
    }

    /// Bounded ranked project-wide lookup (plan section 30 signals).
    fn project_lookup(&self, query: &str) -> Result<ContextBundle, ToolFailure> {
        let query = RetrievalQuery::from_text(query);
        self.retriever.lookup(&query).map_err(|err| ToolFailure {
            code: tool_error::RETRIEVAL_FAILED,
            message: err.to_string(),
        })
    }

    /// Exact symbol lookup, optionally including known relationships.
    /// Argument shape was validated before dispatch.
    fn symbol_lookup(&self, symbol: &str, relations: bool) -> Result<ContextBundle, ToolFailure> {
        let entries = if relations {
            self.retriever.relation_lookup(symbol)
        } else {
            self.retriever.symbol_lookup(symbol)
        }
        .map_err(|err| ToolFailure {
            code: tool_error::RETRIEVAL_FAILED,
            message: err.to_string(),
        })?;
        Ok(assemble_bundle(entries, &ContextBudget::default()))
    }

    /// Read source from the target snapshot only (plan sections 22 and 26).
    /// Argument shape was validated before dispatch.
    fn source_lookup(
        &self,
        path: &str,
        range: Option<SourceRange>,
    ) -> Result<ContextBundle, ToolFailure> {
        let file = self.reader.read(path).map_err(|err| match err {
            GitError::InvalidPath { path } => ToolFailure::invalid_arguments(format!(
                "`path` is not a valid repository-relative snapshot path: `{path}`"
            )),
            other => ToolFailure {
                code: tool_error::GIT_FAILED,
                message: other.to_string(),
            },
        })?;
        let Some(file) = file else {
            return Err(ToolFailure {
                code: tool_error::PATH_NOT_IN_SNAPSHOT,
                message: format!("`{path}` is not part of the target snapshot of this review"),
            });
        };
        let text = String::from_utf8(file.bytes).map_err(|_| ToolFailure {
            code: tool_error::BINARY_CONTENT,
            message: format!("`{path}` is not valid UTF-8 text"),
        })?;

        let Some((line_start, line_end, text)) = select_range(&text, range) else {
            // The requested range starts past the last line: nothing to
            // show, and that is a successful empty answer.
            return Ok(ContextBundle::empty());
        };
        let entry = ContextEntry {
            kind: ContextKind::Source,
            title: path.to_owned(),
            path: Some(path.to_owned()),
            symbol: None,
            line_start,
            line_end,
            record_id: None,
            practice_id: None,
            text,
            score: WEIGHT_SOURCE,
        };
        Ok(assemble_bundle(vec![entry], &ContextBudget::default()))
    }
}

/// Strictly decode one tool's arguments.
fn parse_args<T: for<'de> Deserialize<'de>>(
    arguments: &serde_json::Value,
) -> Result<T, ToolFailure> {
    serde_json::from_value(arguments.clone()).map_err(|err| {
        ToolFailure::invalid_arguments(format!("arguments do not match the schema: {err}"))
    })
}

/// Select the requested lines of a source file.
///
/// Without a range the whole file is returned (`1..=line_count`, or no
/// line numbers for an empty file). With a range the end is clamped to
/// the last line; a range that starts past the last line yields `None`.
/// Returned text preserves the file's exact bytes of the selected lines.
fn select_range(
    text: &str,
    range: Option<SourceRange>,
) -> Option<(Option<u32>, Option<u32>, String)> {
    if let Some(range) = range {
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        let line_count = lines.len() as u32;
        if range.line_start > line_count {
            return None;
        }
        let end = range.line_end.min(line_count);
        let selected: String = lines[(range.line_start - 1) as usize..end as usize].concat();
        return Some((Some(range.line_start), Some(end), selected));
    }
    if text.is_empty() {
        return Some((None, None, String::new()));
    }
    let line_count = text.split_inclusive('\n').count() as u32;
    Some((Some(1), Some(line_count), text.to_owned()))
}

/// Trim a bundle into `remaining` context bytes.
///
/// Entries are dropped from the end (weakest first — the bundle is
/// ranked) until the total fits. When even the strongest entry alone
/// exceeds the remaining budget, a byte-exact prefix of it is returned
/// instead of nothing. An empty result bundle always succeeds without
/// consuming budget; a non-empty result with no remaining budget is the
/// typed `context_budget_exhausted` failure.
fn fit_bundle(mut bundle: ContextBundle, remaining: u64) -> Result<ContextBundle, ToolFailure> {
    if bundle.entries.is_empty() {
        return Ok(bundle);
    }
    if remaining == 0 {
        return Err(ToolFailure {
            code: tool_error::CONTEXT_BUDGET_EXHAUSTED,
            message: "the reviewer consumed all of its returned-context byte budget".to_string(),
        });
    }
    let remaining = usize::try_from(remaining).unwrap_or(usize::MAX);
    let mut truncated = bundle.truncated;
    let strongest = bundle.entries[0].clone();

    while !bundle.entries.is_empty() && bundle.text_bytes > remaining {
        let removed = bundle
            .entries
            .pop()
            .expect("loop condition guarantees an entry");
        bundle.text_bytes -= removed.text.len();
        truncated = true;
    }
    if bundle.entries.is_empty() {
        // Even the strongest entry alone did not fit: serve a bounded
        // prefix of it so the reviewer still receives ordered content.
        let text = truncate_text(&strongest.text, remaining).to_owned();
        if text.len() < strongest.text.len() {
            truncated = true;
        }
        bundle.text_bytes = text.len();
        bundle.entries.push(ContextEntry { text, ..strongest });
    }
    bundle.truncated = truncated;
    Ok(bundle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argument_schemas_validate_shape() {
        assert!(
            ProjectLookupArgs {
                query: "ring buffer".to_string()
            }
            .validate()
            .is_ok()
        );
        assert!(
            ProjectLookupArgs {
                query: String::new()
            }
            .validate()
            .is_err()
        );
        assert!(
            ProjectLookupArgs {
                query: "x".repeat(crate::retrieval::query::MAX_QUERY_BYTES + 1)
            }
            .validate()
            .is_err()
        );

        assert!(
            SymbolArgs {
                symbol: "enqueue".to_string()
            }
            .validate()
            .is_ok()
        );
        assert!(
            SymbolArgs {
                symbol: String::new()
            }
            .validate()
            .is_err()
        );
        assert!(
            SymbolArgs {
                symbol: "x".repeat(MAX_SYMBOL_BYTES + 1)
            }
            .validate()
            .is_err()
        );

        assert!(
            SourceLookupArgs {
                path: "src/queue.rs".to_string(),
                range: None,
            }
            .validate()
            .is_ok()
        );
        assert!(
            SourceLookupArgs {
                path: "src/queue.rs".to_string(),
                range: Some(SourceRange {
                    line_start: 2,
                    line_end: 4,
                }),
            }
            .validate()
            .is_ok()
        );
        assert!(
            SourceLookupArgs {
                path: String::new(),
                range: None,
            }
            .validate()
            .is_err()
        );
        assert!(
            SourceLookupArgs {
                path: "x".repeat(MAX_PATH_BYTES + 1),
                range: None,
            }
            .validate()
            .is_err()
        );
        assert!(
            SourceLookupArgs {
                path: "src/queue.rs".to_string(),
                range: Some(SourceRange {
                    line_start: 4,
                    line_end: 2,
                }),
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn argument_schemas_reject_unknown_fields_and_wrong_types() {
        assert!(
            serde_json::from_value::<ProjectLookupArgs>(serde_json::json!({
                "query": "x",
                "symbol": "y",
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<SymbolArgs>(serde_json::json!({
                "symbol": 42,
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<SourceLookupArgs>(serde_json::json!({
                "path": "src/a.rs",
                "line": 3,
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<SourceRange>(serde_json::json!({
                "line_start": 1,
                "line_end": 2,
                "step": 1,
            }))
            .is_err()
        );
        // A JSON null range is accepted as absence, matching the host's
        // optional-field spelling.
        let args: SourceLookupArgs =
            serde_json::from_value(serde_json::json!({"path": "a.rs", "range": null})).unwrap();
        assert!(args.range.is_none());
    }

    #[test]
    fn source_range_validates_shape() {
        assert!(
            SourceRange {
                line_start: 1,
                line_end: 1
            }
            .validate()
            .is_ok()
        );
        assert!(
            SourceRange {
                line_start: 3,
                line_end: 9
            }
            .validate()
            .is_ok()
        );
        for bad in [
            SourceRange {
                line_start: 0,
                line_end: 1,
            },
            SourceRange {
                line_start: 5,
                line_end: 4,
            },
        ] {
            let err = bad.validate().unwrap_err();
            assert_eq!(err.code, tool_error::INVALID_ARGUMENTS);
        }
    }

    #[test]
    fn select_range_preserves_exact_bytes_and_clamps() {
        let text = "one\ntwo\r\nthree";
        // Whole file: three lines, exact bytes.
        let (start, end, selected) = select_range(text, None).unwrap();
        assert_eq!((start, end), (Some(1), Some(3)));
        assert_eq!(selected, text);

        // Range keeps the carriage return of line two.
        let (start, end, selected) = select_range(
            text,
            Some(SourceRange {
                line_start: 2,
                line_end: 2,
            }),
        )
        .unwrap();
        assert_eq!((start, end), (Some(2), Some(2)));
        assert_eq!(selected, "two\r\n");

        // End is clamped to the last line.
        let (start, end, selected) = select_range(
            text,
            Some(SourceRange {
                line_start: 2,
                line_end: 99,
            }),
        )
        .unwrap();
        assert_eq!((start, end), (Some(2), Some(3)));
        assert_eq!(selected, "two\r\nthree");

        // Past the last line: nothing to show.
        assert!(
            select_range(
                text,
                Some(SourceRange {
                    line_start: 4,
                    line_end: 5,
                })
            )
            .is_none()
        );

        // Empty file: no line numbers, empty text.
        let (start, end, selected) = select_range("", None).unwrap();
        assert_eq!((start, end), (None, None));
        assert_eq!(selected, "");
    }

    fn entry(text: &str, score: f64) -> ContextEntry {
        ContextEntry {
            kind: ContextKind::Source,
            title: text.to_string(),
            path: None,
            symbol: None,
            line_start: None,
            line_end: None,
            record_id: None,
            practice_id: None,
            text: text.to_string(),
            score,
        }
    }

    fn bundle_of(texts: &[&str]) -> ContextBundle {
        let candidates: Vec<ContextEntry> = texts
            .iter()
            .enumerate()
            .map(|(index, text)| entry(text, 1.0 - index as f64 * 0.1))
            .collect();
        assemble_bundle(candidates, &ContextBudget::default())
    }

    #[test]
    fn fit_bundle_trims_weakest_entries_first() {
        let bundle = bundle_of(&["aaaa", "bbbb", "cccc"]);
        // Enough room for two of the three entries.
        let fitted = fit_bundle(bundle.clone(), 8).unwrap();
        assert_eq!(fitted.entries.len(), 2);
        assert_eq!(fitted.entries[0].text, "aaaa");
        assert_eq!(fitted.entries[1].text, "bbbb");
        assert!(fitted.truncated);
        assert_eq!(fitted.text_bytes, 8);
    }

    #[test]
    fn fit_budget_exact_fit_is_not_marked_truncated() {
        let bundle = bundle_of(&["aaaa", "bbbb"]);
        let fitted = fit_bundle(bundle, 8).unwrap();
        assert_eq!(fitted.entries.len(), 2);
        assert!(!fitted.truncated);
    }

    #[test]
    fn fit_bundle_prefixes_the_strongest_entry_when_nothing_fits_whole() {
        let bundle = bundle_of(&["aaaaaa", "bbbbbb"]);
        let fitted = fit_bundle(bundle, 3).unwrap();
        assert_eq!(fitted.entries.len(), 1);
        assert_eq!(fitted.entries[0].text, "aaa");
        assert!(fitted.truncated);
        assert_eq!(fitted.text_bytes, 3);
    }

    #[test]
    fn fit_bundle_never_splits_characters() {
        let bundle = bundle_of(&["åäö"]);
        let fitted = fit_bundle(bundle, 2).unwrap();
        assert_eq!(fitted.entries[0].text, "å");
        assert_eq!(fitted.text_bytes, 2);
    }

    #[test]
    fn fit_bundle_rejects_nonempty_results_without_budget() {
        let err = fit_bundle(bundle_of(&["a"]), 0).unwrap_err();
        assert_eq!(err.code, tool_error::CONTEXT_BUDGET_EXHAUSTED);
        // Empty results are always fine.
        let fitted = fit_bundle(ContextBundle::empty(), 0).unwrap();
        assert!(fitted.is_empty());
    }

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(tool_error::UNKNOWN_REVIEW, "unknown_review");
        assert_eq!(tool_error::UNKNOWN_REVIEWER, "unknown_reviewer");
        assert_eq!(tool_error::CAPABILITY_DENIED, "capability_denied");
        assert_eq!(
            tool_error::LOOKUP_BUDGET_EXHAUSTED,
            "lookup_budget_exhausted"
        );
        assert_eq!(
            tool_error::CONTEXT_BUDGET_EXHAUSTED,
            "context_budget_exhausted"
        );
        assert_eq!(tool_error::INVALID_ARGUMENTS, "invalid_arguments");
        assert_eq!(tool_error::PATH_NOT_IN_SNAPSHOT, "path_not_in_snapshot");
        assert_eq!(tool_error::BINARY_CONTENT, "binary_content");
        assert_eq!(tool_error::RETRIEVAL_FAILED, "retrieval_failed");
        assert_eq!(tool_error::GIT_FAILED, "git_failed");
    }
}
