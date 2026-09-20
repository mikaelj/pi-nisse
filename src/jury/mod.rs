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
//!   strict synthesis-output validation (plan section 38).

pub mod catalog;
pub mod execution;
pub mod findings;
pub mod host;
pub mod plan;
pub mod protocol;
pub mod synthesis;
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
pub use synthesis::{ConsolidationOptions, SYNTHESIZER_ID, consolidate};
pub use tools::ReviewerToolServer;
