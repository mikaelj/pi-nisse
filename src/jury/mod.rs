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
//!   host child process (plan section 21);
//! - [`host`] spawns and drives that child process with typed failures,
//!   timeouts, and clean shutdown;
//! - [`tools`] serves the read-only reviewer lookup tools on the Rust
//!   side of that protocol, snapshot-bound and budget-enforced (plan
//!   section 22);
//! - [`execution`] applies the deterministic reviewer retry policy on top
//!   of the host exchange (plan section 36).

pub mod catalog;
pub mod execution;
pub mod host;
pub mod plan;
pub mod protocol;
pub mod tools;

pub use catalog::{Catalog, CatalogError};
pub use execution::execute_review;
pub use host::{HostError, HostProcess, PiHostLaunch, ReviewerOutcome};
pub use plan::{
    JuryPlanError, MAX_JURY_SIZE, MAX_SELECTED_SPECIALISTS, MIN_JURY_SIZE, build_jury_plan,
};
pub use tools::ReviewerToolServer;
