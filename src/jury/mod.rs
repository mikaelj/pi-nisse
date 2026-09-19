//! Jury composition.
//!
//! - [`catalog`] bundles and validates the reviewer profiles and
//!   practice packs;
//! - [`plan`] constructs the bounded [`JuryPlan`] for a routing
//!   decision: `quality` always participates, at most three scored
//!   specialists join, and the dynamic domain expert is the
//!   deterministic fallback when no specialist reaches the routing
//!   threshold.

pub mod catalog;
pub mod plan;

pub use catalog::{Catalog, CatalogError};
pub use plan::{
    JuryPlanError, MAX_JURY_SIZE, MAX_SELECTED_SPECIALISTS, MIN_JURY_SIZE, build_jury_plan,
};
