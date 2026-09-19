//! Reviewer execution policy over the Pi review host (plan section 36).
//!
//! The host process executes reviewer sessions; deterministic review policy
//! belongs to Rust. This module drives one full review exchange and applies
//! the frozen retry rule:
//!
//! - a failed reviewer may be retried exactly once;
//! - the retry uses the same [`ReviewerExecutionSpec`] — model, reviewer,
//!   checklist, and capabilities are never silently changed;
//! - outcomes of the retry replace the failed first attempt.
//!
//! The minimum-valid-reports rule (at least two valid independent reports)
//! is review-pipeline policy and belongs to later milestones.

use std::collections::BTreeMap;
use std::time::Duration;

use super::host::{HostError, HostProcess, ReviewerOutcome};
use super::protocol::{StartReviewPayload, ToolRequestPayload, ToolResponsePayload};

/// Drive one review to completion with the one-retry policy.
///
/// Sends `start_review` for every reviewer in `payload`, waits for all of
/// them to complete or fail, then retries every failed reviewer once with
/// its identical execution spec. Tool requests of all rounds are answered
/// through `tool_handler`. `timeout` bounds each round independently (each
/// reviewer session additionally enforces its own per-spec timeout).
///
/// The returned outcomes keep the reviewer order of `payload`.
pub fn execute_review(
    host: &mut HostProcess,
    payload: &StartReviewPayload,
    tool_handler: &mut dyn FnMut(&ToolRequestPayload) -> ToolResponsePayload,
    timeout: Duration,
) -> Result<Vec<(String, ReviewerOutcome)>, HostError> {
    let review_id = payload.review_id.clone();
    let expected: Vec<String> = payload
        .reviewers
        .iter()
        .map(|s| s.reviewer_id.clone())
        .collect();

    host.start_review(payload)?;
    let first = host.complete_review(&review_id, &expected, tool_handler, timeout)?;

    let mut outcomes: BTreeMap<String, ReviewerOutcome> = first.into_iter().collect();

    let failed: Vec<String> = outcomes
        .iter()
        .filter(|(_, outcome)| matches!(outcome, ReviewerOutcome::Failed(_)))
        .map(|(reviewer_id, _)| reviewer_id.clone())
        .collect();
    if !failed.is_empty() {
        // Retry round: identical specs, same review id, fresh round budget.
        let retry_specs: Vec<_> = payload
            .reviewers
            .iter()
            .filter(|spec| failed.contains(&spec.reviewer_id))
            .cloned()
            .collect();
        let retry_payload = StartReviewPayload {
            review_id: payload.review_id.clone(),
            kind: payload.kind,
            reviewers: retry_specs,
        };
        host.start_review(&retry_payload)?;
        let retried = host.complete_review(&review_id, &failed, tool_handler, timeout)?;
        for (reviewer_id, outcome) in retried {
            outcomes.insert(reviewer_id, outcome);
        }
    }

    let mut resolved: Vec<(String, ReviewerOutcome)> = Vec::with_capacity(expected.len());
    for reviewer_id in expected {
        let outcome =
            outcomes
                .remove(&reviewer_id)
                .ok_or_else(|| HostError::UnexpectedHostMessage {
                    reason: format!("review `{review_id}` never resolved reviewer `{reviewer_id}`"),
                })?;
        resolved.push((reviewer_id, outcome));
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_specs_keep_identity_and_order_of_failures() {
        // Structural sanity for the retry payload construction: the helper
        // filters by identity and clones specs unchanged. The full retry
        // behavior is covered by the process integration tests.
        let payload = StartReviewPayload {
            review_id: "rev".to_string(),
            kind: crate::domain::review::ReviewKind::Plan,
            reviewers: vec![
                crate::domain::reviewer::ReviewerExecutionSpec {
                    reviewer_id: "quality".to_string(),
                    specialization: "general".to_string(),
                    instructions: "i".to_string(),
                    checklist: vec!["c".to_string()],
                    model_profile: "standard".to_string(),
                    thinking_level: crate::domain::model::ThinkingLevel::Medium,
                    initial_context: vec![],
                    allowed_capabilities: vec![],
                    max_lookup_calls: 0,
                    max_returned_context_bytes: 0,
                    timeout_ms: 1_000,
                },
                crate::domain::reviewer::ReviewerExecutionSpec {
                    reviewer_id: "security".to_string(),
                    specialization: "security".to_string(),
                    instructions: "i".to_string(),
                    checklist: vec!["c".to_string()],
                    model_profile: "strong".to_string(),
                    thinking_level: crate::domain::model::ThinkingLevel::High,
                    initial_context: vec![],
                    allowed_capabilities: vec![],
                    max_lookup_calls: 0,
                    max_returned_context_bytes: 0,
                    timeout_ms: 1_000,
                },
            ],
        };
        let failed = ["security".to_string()];
        let retry_specs: Vec<_> = payload
            .reviewers
            .iter()
            .filter(|spec| failed.contains(&spec.reviewer_id))
            .cloned()
            .collect();
        assert_eq!(retry_specs.len(), 1);
        assert_eq!(retry_specs[0].reviewer_id, "security");
        assert_eq!(retry_specs[0].model_profile, "strong");
        assert_eq!(retry_specs, payload.reviewers[1..2].to_vec());
    }
}
