//! Jury composition and reviewer execution.
//!
//! - [`catalog`] bundles and validates the reviewer profiles and
//!   practice packs;
//! - [`plan`] constructs the bounded [`JuryPlan`] for a routing
//!   decision: `quality` always participates, at most three scored
//!   specialists join, and the dynamic domain expert is the
//!   deterministic fallback when no specialist reaches the routing
//!   threshold;
//! - [`protocol`] is the frozen JSONL protocol spoken with the Pi review
//!   host child process (plan section 21), including the narrow finding
//!   equivalence check of finding consolidation;
//! - [`host`] spawns and drives that child process with typed failures,
//!   timeouts, and clean shutdown;
//! - [`tools`] serves the read-only reviewer lookup tools on the Rust
//!   side of that protocol, snapshot-bound and budget-enforced (plan
//!   section 22);
//! - [`execution`] applies the deterministic reviewer retry policy on top
//!   of the host exchange (plan section 36);
//! - [`findings`] validates reviewer findings and evidence against the
//!   exact snapshot, applies the severity policy, fingerprints valid
//!   findings, and groups obvious duplicates (plan sections 14, 15, 37);
//! - [`synthesis`] consolidates validated findings into final findings:
//!   narrow equivalence checks, the non-juror synthesis session, and
//!   strict synthesis-output validation (plan section 38);
//! - [`review`] orchestrates the complete review pipeline over all of
//!   the above (plan sections 58–60): snapshot, index refresh,
//!   retrieval, routing, jury execution, validation, synthesis, and
//!   the local review event — for plan reviews (section 59) and code
//!   reviews of every snapshot mode (section 60).

pub mod catalog;
pub mod execution;
pub mod findings;
pub mod host;
pub mod investigation;
pub mod plan;
pub mod protocol;
pub mod review;
pub mod synthesis;
pub mod thinking;
pub mod tools;

pub use catalog::{Catalog, CatalogError};
pub use execution::execute_review;
pub use findings::{
    FindingGroup, RejectedFinding, ReportValidation, ValidatedFinding, ValidationContext,
    apply_equivalence, equivalence_candidates, group_findings, validate_report,
};
pub use host::{HostError, HostProcess, PiHostLaunch, ReviewerOutcome};
pub use plan::{
    JuryPlanError, MAX_JURY_SIZE, MAX_SELECTED_SPECIALISTS, MIN_JURY_SIZE, build_jury_plan,
};
pub use review::{CodeReviewError, CodeReviewOptions, CodeReviewOutcome};
pub use review::{
    MAX_PLAN_BYTES, PlanInput, PlanReviewError, PlanReviewOptions, PlanReviewOutcome,
    host_launch_from_env, render_review_text, run_code_review, run_plan_review,
};
pub use synthesis::{ConsolidationOptions, SYNTHESIZER_ID, consolidate};
pub use tools::ReviewerToolServer;
